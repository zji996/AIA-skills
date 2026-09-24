---
name: repo-governance
description: 仓库上下文治理。当需要搭建或整理 AGENTS.md、docs/current.md、决策记录，或发现文档与代码对不上、上下文开始漂移时使用；附带只读审计脚本定位问题。Use for repo context governance, AGENTS.md setup, docs drift audit.
license: MIT
metadata:
  version: "2.0.0"
---

# Repo Governance (仓库上下文治理)

目标是让任何新会话或新 Agent 都能用最少的阅读量拿到正确的项目上下文。不规定语言、框架或代码组织方式。

## 先审计

```bash
<本技能目录>/scripts/audit-context.py [--repo <dir>] [--max-agents-lines 150] [--stale-days 30]
```

只读，每个问题输出一行 `WARN <文件>: <原因>`，有问题时退出码为 1。检查项：入口文件是否缺失或过长、`docs/current.md` 的下一步是否过多或长期未更新、`.local/` 是否被 gitignore、Markdown 相对链接是否断裂。根据结果决定要不要动文档，不要为了"齐全"而补文件。

## 两条原则

1. **代码事实优先**：文档和运行态配置、代码冲突时以后者为准，并顺手修正文档。文档只是缓存，过期的缓存比没有更糟。
2. **渐进式沉淀**：只有需要跨会话保留的信息才落盘，不预先铺空目录和占位文档；每多一个文件，接手者就多一份阅读和核对成本。

## 信息分层

| 位置 | 放什么 | 为什么放这里 |
| --- | --- | --- |
| `AGENTS.md`（或等价入口） | 项目目标、关键命令、验证入口、高风险禁区 | 每个会话都会自动加载，越短越不容易被忽略 |
| `docs/current.md` | 当前焦点、阻塞项、≤5 条下一步 | 长任务的断点；条目一多就失去"下一步"的意义，完成即清理 |
| `docs/reference/` | 已验证的稳定架构事实 | 与规划混在一起会误导接手者，规划内容需标注 *Proposed* |
| `docs/decision/` | 影响深远的选型、备选方案与理由 | 代码能说明"是什么"，说明不了"为什么没选别的" |
| `docs/roadmap.md` | 长期方向与里程碑 | 与当前任务解耦，避免 `current.md` 膨胀 |
| `.local/`（整体 gitignore） | 运行产物、任务草稿 `.local/plan/` | 临时内容不进版本库；稳定结论在任务结束前回填到上面几层 |

小项目只需要 `AGENTS.md`；出现跨会话的长任务再加 `docs/current.md`；其余层按需出现。
