---
name: iteration-speed
description: 新机器或新仓库初始化、构建/测试/门禁变慢、设置门禁预算、多个 worktree 或 Agent 并行开发、决定验收分层或模块拆分时使用；测量反馈回路、监测门禁预算、探测缓存并配置用户级 Rust 加速。Use when initializing a machine/repository, diagnosing slow build/test gates, enforcing gate budgets, sharing caches across worktrees or parallel agents, or choosing validation layers and module boundaries.
metadata:
  version: "1.1.0"
---

# 仓库迭代速度

先量，再改。机器相关的工具路径、链接器、缓存容量只放用户级；仓库保留与机器无关的 profile、项目引用和验收入口，避免换机器就不能构建。

## 判断点

1. **量反馈回路**：同一环境记录无改动重跑、改下游一处、改上游一处、完整门禁的耗时，区分冷启动和热缓存。结合构建日志判断依赖编译、自有代码、链接、测试执行或 I/O 等待；只看全量时间会优化错地方；多核机器上冷编译依赖往往不是瓶颈，测试执行和门禁排队更常见。
2. **分层验收**：迭代跑与改动相关的按包/按文件测试及类型检查；完整门禁按批在合入后跑一次，记录对应版本。仓库规定的必跑门禁仍须执行。并行 Agent 的重检查整机串行，重复全量会争抢 CPU 并放大等待。
3. **跨 worktree 缓存**：先确认共享 store/内容寻址缓存能命中哪些产物。下载依赖可共享，不代表增量状态、路径敏感的自有代码、链接产物能共享；不要把并行写入的构建目录硬绑到一起。
4. **分开模式产物**：避免来回切换 Rust check/build、TS noEmit/emit 或 feature/参数组合；若实测互相失效，固定迭代模式或使用用户级独立输出目录。隔离会多占空间，不能当作免费加速。
5. **缩小重编/重测范围**：依赖链上游的大模块改一处会带动下游；结合最长链、各段时间和实际修改频率，测过有收益再拆，不为“解耦”预先拆分。
6. **测试本身**：只跑受影响的测试，慢测试分组；并行度按整机容量设置。链接或启动占主导时合并碎测试二进制/入口；共享状态与隔离损失可能抵消收益，须复测。
7. **门禁预算**：完整门禁设耗时预算，用 `gate` 跑；超预算即排优化，先量最慢的步骤或测试，再按上述判断点改动并复测。

## 工具与按需参考

在目标仓库运行 `<本技能目录>/scripts/iteration-speed detect`，只读报告现状；静态探测的未解析项需人工确认。
用 `time -- <命令>` 记录耗时，`history` 查看最近 20 条；日志不应包含凭据。
用 `gate --name full --budget 10m -- <命令>` 运行门禁（需 Python 3），预算支持秒数或 `ms/s/m/h/d`，可组合如 `1m30s`。
省略 `--budget` 时读取仓库根 `.iteration-speed.json`，例如 `{"gates":{"full":"10m"}}`；名称默认 `default`，无预算只记录。
命令退出码透传；命令成功但超预算返回 3。相较同名门禁最近最多 5 次命令成功记录的中位数慢超过 30% 时提示回退，不另改退出码。
识别 Cargo 输出时列最慢的 5 个测试二进制及 `--report-time` 测试；自定义输出无法配对时跳过。历史沿用用户级 TSV，门禁行第 4 列为 `gate`、第 5 列为名称、第 6 列为数字摘要，不存命令参数、原始输出或测试名称。
重检查排队时把本工具的 `time`/`gate` 放在队列内，避免把排队时间算进反馈时间。
`rust-setup --print` 预览用户配置，`--write` 写入并备份；工具安装命令只打印。已有复杂 TOML 配置拒绝自动合并时，按预览人工调整。

只读相关生态：[Rust](references/rust.md)、[Node](references/node.md)、[Python](references/python.md)、[Go](references/go.md)、[Docker](references/docker.md)。
