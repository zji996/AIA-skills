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
| **`repo-governance`** | `skills/repo-governance/` | **上下文治理与审计**：定义 `AGENTS.md`、`docs/current.md`、决策记录的信息分层；`audit-context.py` 只读检查入口文件过长、下一步堆积、`.local/` 未忽略、文档断链等漂移问题。 |
| **`agent-handoff`** | `skills/agent-handoff/` | **会话交接**：`handoff-snapshot.sh` 自动采集分支、HEAD、未提交文件、最近提交与未读取的委派任务，生成交接账本草稿，模型只需补充判断部分。 |
| **`pi-delegation`** | `skills/pi-delegation/` | **Pi 后台协作**：异步启动多个 Pi 任务（写代码、只读评审、第二意见），分段 `wait` 适配工具时长上限；写入互斥、进度流过滤、完整答复留存；禁止嵌套委派，且不会安装给 Pi 自己。 |
| **`openai-image-gen`** | `skills/openai-image-gen/` | **图像生成落盘**：调用 OpenAI Image API 生成配图、Banner、图标等素材，直接写入本地文件并只返回一行 JSON；附提示词、尺寸与费用选择要点。 |

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
curl -fsSL <上面任一链接> | bash -s -- --ref v2.0.0              # 安装指定版本
curl -fsSL <上面任一链接> | bash -s -- --copy                    # 拷贝安装
curl -fsSL <上面任一链接> | bash -s -- agent-handoff             # 只安装指定技能
curl -fsSL <上面任一链接> | bash -s -- --github --dir ~/aia-skills # 指定来源与位置
```

依赖 `git` 与 bash 4+（macOS 需先 `brew install bash`）。`pi-delegation` 仅支持 Linux。

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
```

**安装模式**：开发机用默认的符号链接，只存一份源码，保存即生效，`git pull` 就是更新。服务器、容器或其他机器用 `--copy`，每份拷贝带 `.aia-skills-install` 标记，记录来源、版本和提交号，`--status` 据此判断是否过期；更新时重新运行 `--copy` 即可。两种模式可以互相切换。

**安装目录**：

| 目录 | 读取它的 Agent |
| --- | --- |
| `~/.agents/skills/` | Pi、Codex、Cursor、Kilo |
| `~/.claude/skills/` | Claude Code |

普通技能只装到这两个目录。在 frontmatter `metadata.exclude-agents` 中声明了排除对象的技能（目前只有 `pi-delegation` 排除了 `pi`，防止 Pi 调度自己）会跳过 `~/.agents/skills/`，改为装到其余 Agent 各自的目录：`~/.codex/skills/`、`~/.cursor/skills/`、`~/.kilo/skills/`。

安装脚本只处理本仓库安装的条目：不会覆盖别人的目录；会移除多余位置上的旧条目，也会清理源技能已删除的条目。退役技能时直接从 `skills/` 删除，重新运行安装脚本即可清理干净。

### 技能间约定

技能彼此独立，唯一的数据约定是：`agent-handoff` 的快照脚本读取 `pi-delegation` 写在 `.local/run/pi/<run_id>/` 下的 `meta.json`、`exit_code` 与 `.delivered` 来判断未完成的委派任务。修改这些文件名或含义时要同步修改 `handoff-snapshot.sh` 及其测试。

### 版本与发布

- 每个技能的 `metadata.version` 按语义化版本维护：只改文档写法升修订号，新增能力升次版本号，命令、参数或文件格式不兼容时升主版本号。
- 改动记入 [CHANGELOG.md](CHANGELOG.md) 的"未发布"一节；发布时把它改成版本号和日期，提交后打 `git tag vX.Y.Z`。
- 拷贝安装的机器更新到某个版本：`git checkout vX.Y.Z && ./scripts/install.sh --copy`。

### 验证

```bash
./scripts/check.sh
python3 -m unittest discover -s tests -v
```

`check.sh` 检查 frontmatter、`description` 是否写明触发场景、README 索引、`evals/` 触发示例、断链与脚本语法。依赖 Python 3.11+ 和 PyYAML；图像生成脚本另需 `curl`、`jq` 和有效的 OpenAI API key，调用会产生 API 费用。

---

## 📄 授权与许可

本项目采用 [MIT License](LICENSE) 开源。
