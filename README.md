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
| **`repo-governance`** | `skills/repo-governance/` | **上下文治理与审计**：定义 `AGENTS.md`、`docs/current.md`、决策记录的信息分层；`audit-context.py` 按仓库配置估算 token 预算与上下文健康度，支持 `--report`、`--only`，并检查下一步堆积、`.local/` 未忽略、文档断链、`PROVISIONAL(Cnn)` 标记缺少待收敛清单条目等漂移问题。 |
| **`agent-handoff`** | `skills/agent-handoff/` | **会话交接**：`handoff-snapshot.sh` 自动采集分支、HEAD、未提交文件、最近提交、`.local/run/delegate/` 与旧 `.local/run/pi/` 的未读取委派任务和未合并 worktree，生成交接账本草稿。 |
| **`delegate`** | `skills/delegate/` | **同事 Agent 委派**：拆分任务、后台运行 Pi/Codex，按会话收取短结论与答复，完整字段按需 `--json`；每任务一个等待者，重复等待退出 76。`--accept-also` 追加任务测试并保留默认验收，覆盖默认时提示。`agentDeny` 用前缀或精确 argv 拦截同事全量检查（含 lane），`evidence` 收集评测且不改交付状态；timeout 后 reply 续做不耗返工次数。写入用 `--worktree` 隔离、`--protect-reason` 划分所有权，生成路径收尾核对；托管 copy/link 与未初始化参考 gitlink 的内容列为 `excluded`。`apply` 支持生成重试、编号预警、`acceptStillValid` 与合并后复验；手动合入自动识别，`pendingChanges` 保留累计待合入量。写入收尾缓存 worktree 与源构建目录占用，status/wait 达阈值独立提示。Rust 管理整机与仓库两层容量、lane、后台进程回收，固定同事显示默认档位，长答复全文落文件；[长时间自主推进](skills/delegate/references/long-run.md) 按需读取，实战经验按版本压缩沉淀。 |
| **`openai-image-gen`** | `skills/openai-image-gen/` | **图像生成落盘**：先用便宜模型打草稿定构图，再以草稿为参考出一次正式图；每个输出文件限定草稿 3、正式 1、编辑 1 次，超出需用户同意；只返回一行 JSON；附示意图提示词结构与审查要点。 |
| **`scroll-gesture`** | `skills/scroll-gesture/` | **滚动手势**：`input-probe.html` 在目标环境（Windows 鼠标、远程桌面、触屏）实测滚轮与触摸事件的频率、间隔、增量与鼠标格占比，一键复制 JSON；附过界翻页等自定义手势的判断点：时间只区分惯性、结果交给位置、动画可打断、鼠标格缓动。 |
| **`iteration-speed`** | `skills/iteration-speed/` | **仓库迭代速度**：新机器/仓库初始化、构建测试门禁变慢、多 worktree/Agent 并行或验收分层与模块拆分前使用；`iteration-speed detect` 只读探测生态与缓存配置，`time`/`history` 记录反馈时间，`gate` 监测门禁预算与耗时回退、汇总最慢 Cargo 测试，`rust-setup` 预览或备份后写入用户级 sccache/mold 配置；附 Rust、Node、Python、Go、Docker 的缓存边界与实践。 |

`delegate` 架构总览：[docs/assets/delegate-architecture.webp](docs/assets/delegate-architecture.webp)（提示词同目录）。

`delegate` 5.25.0 少写少选：仓库 `standing` 固定说明与 `agentDeny` 提示自动附到每份任务说明，`defaults` 给 worktree/protect/acceptAlso/timeout/evidence 默认值并在启动时回显来源；省略 `--name` 取说明首行；无参 `wait` 交付下一个结束的任务并在末行写明还剩几个、再跑哪条命令；答复显示上限默认 20000（`resultChars`），`--max-answer` 超长时自动压缩一次；`apply` 合入后自动清理并说明验收是否仍有效；任务收尾不再终止 sccache 这类共享守护进程。入口 `SKILL.md` 精简为三步、场景表与档位，状态与边界细节移到 `references/reading-results.md`。

`delegate` 5.24.0 源构建目录提示：写入收尾在 worktree 测量后限时测源仓库的现存 target（根有 Cargo.toml），用户/仓库 `sourceBuild.paths` 可替换、空数组关闭；status/wait 取同源最近缓存，默认 60 GiB 提示总量、各路径大小及[先量再删的参考](skills/delegate/references/cleanup.md)，`DELEGATE_SOURCE_BUILD_WARN_GIB` 可调、0 关闭，JSON 独立追加 `sourceBuildDisk`；失败不改结论，只提示不删。

`delegate` 5.23.0 清理能力：移除 worktree 连带回收 compose 容器和匿名卷，`clean` 同时回收目录已消失的旧孤儿，Docker 失败只提示；写入任务结束时缓存磁盘占用，`status`/`wait` 达默认 20 GiB 时提示总量、最大三项与清理命令（`DELEGATE_WORKTREE_WARN_GIB` 可调、0 关闭）。改写后手动合入并提交立即 `clean <run> --force`；门禁直接后台跑命令本身或 `lane`，避免 `pgrep -f` 轮询匹配自身。

`delegate` 5.18.0 配置能力：用户级 `${XDG_CONFIG_HOME}/delegate/config.json`（未设变量时使用系统约定的用户配置目录）与仓库 `.delegate.json` 合并，通用 deny/env/返工预算/默认验收与证据只写一次；`--evidence` 失败不改状态，deny 支持精确 argv，手动合入自动检测，固定同事仍显示默认档位。整机容量默认 12/6/2、每仓库 8/4（worktree 同计），环境变量优先；仓库可按 `(argv, exact)` 撤销用户 deny，`configSources` 可追溯来源。规则见 [用户与仓库配置](skills/delegate/references/configuration.md)。

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
curl -fsSL <上面任一链接> | bash -s -- --ref v3.0.0              # 安装指定版本
curl -fsSL <上面任一链接> | bash -s -- --copy                    # 拷贝安装
curl -fsSL <上面任一链接> | bash -s -- agent-handoff             # 只安装指定技能
curl -fsSL <上面任一链接> | bash -s -- --github --dir ~/aia-skills # 指定来源与位置
curl -fsSL <上面任一链接> | bash -s -- --with-pi                 # 同时安装或更新 Pi（delegate 需要）
```

依赖 `git` 与 bash 4+（macOS 需先 `brew install bash`）。`delegate` 仅支持 Linux x86_64 / aarch64：安装时按 `skills/delegate/bin.sha256` 校验并下载 GitHub Release 中的静态二进制；下载不到而本机有 cargo 时从 `crates/delegate` 编译，不匹配校验和会提示未发布校验，`AIA_SKILLS_REQUIRE_VERIFIED=1` 可拒绝该构建。另需所选同事的 CLI：`pi` 或 `codex`。

### Pi 与 pi-kit

`delegate` 调度的 Pi 由子模块 [`third_party/pi-kit`](third_party/pi-kit) 安装。子模块地址是相对地址，从主仓库克隆时指向主仓库的 pi-kit，从 GitHub 克隆时指向 GitHub 上的 pi-kit。

- `--with-pi`：拉取子模块并运行 `pi-kit --additive`，安装或升级 Pi 与 pi-kit 管理的 Pi 包，不改动现有设置；没有 Node.js 22.19+ 时会免 root 安装便携版。完成后提示是否缺少 `python3`。
- `--with-pi-sync`：改为运行 `pi-kit --sync`，应用 pi-kit 的完整声明式配置，保留 `defaultProvider`、`defaultModel` 等本机设置。
- 需要镜像加速时可设置 `PI_KIT_MIRROR=cn` 强制使用 npmmirror。

修改 pi-kit 时直接在 `third_party/pi-kit` 中提交，并先推送 pi-kit；然后在本仓库提交子模块指针，否则别人拉不到指针指向的提交。

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
./scripts/install-git-hooks.sh                         # 本仓库启用 pre-push 门禁
./scripts/sync-hosts.sh                               # 经 SSH 并行更新其他机器（列表在 ~/.config/aia-skills/hosts）
```

**安装模式**：开发机用默认的符号链接，只存一份源码，保存即生效，`git pull` 就是更新。服务器、容器或其他机器用 `--copy`，每份拷贝带 `.aia-skills-install` 标记，记录来源、版本和提交号，`--status` 据此判断是否过期；更新时重新运行 `--copy` 即可。两种模式可以互相切换。

**安装目录**：

| 目录 | 读取它的 Agent |
| --- | --- |
| `~/.agents/skills/` | Pi、Codex、Cursor、Kilo |
| `~/.claude/skills/` | Claude Code |

普通技能只装到这两个目录。在 frontmatter `metadata.exclude-agents` 中声明了排除对象的技能（目前只有 `delegate` 排除了 `pi`，防止 Pi 调度自己）会跳过 `~/.agents/skills/`，改为装到其余 Agent 各自的目录：`~/.codex/skills/`、`~/.cursor/skills/`、`~/.kilo/skills/`。

安装脚本只处理本仓库安装的条目：不会覆盖别人的目录；会移除多余位置上的旧条目，也会清理源技能已删除的条目。退役技能时直接从 `skills/` 删除，重新运行安装脚本即可清理干净。

### 技能间约定

技能彼此独立，唯一的数据约定是：`agent-handoff` 的快照脚本读取 `delegate` 写在 `.local/run/delegate/<run_id>/`（也兼容旧 `.local/run/pi/`）下的 `meta.json`（含 `worktree.path`）、`exit_code`、`.delivered` 与 `.applied` 来判断未完成任务和未合并 worktree。修改这些文件名或含义时要同步修改 `handoff-snapshot.sh` 及其测试。

### 版本与发布

- 每个技能的 `metadata.version` 按语义化版本维护：只改文档写法升修订号，新增能力升次版本号，命令、参数或文件格式不兼容时升主版本号。
- 改动记入 [CHANGELOG.md](CHANGELOG.md) 的"未发布"一节；发布时把它改成版本号和日期，提交后打 `git tag vX.Y.Z`。
- 拷贝安装的机器更新到某个版本：`git checkout vX.Y.Z && ./scripts/install.sh --copy`。

### 验证

```bash
./scripts/verify.sh   # check.sh、单元测试；有 cargo 时运行 crates/delegate 的 clippy
```

`check.sh` 检查 frontmatter、`description` 是否含中英文触发场景、README 索引、`evals/` 触发示例、断链与脚本语法。`install-git-hooks.sh` 只设置本仓库的 `core.hooksPath`，pre-push 会调用 `verify.sh`。依赖 Python 3.11+ 和 PyYAML；图像生成脚本另需 `curl`、`jq` 和有效的 OpenAI API key，调用会产生 API 费用。

---

## 📄 授权与许可

本项目采用 [MIT License](LICENSE) 开源。
