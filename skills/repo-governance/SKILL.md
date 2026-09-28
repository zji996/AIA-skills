---
name: repo-governance
description: 仓库上下文治理。当需要搭建或整理 AGENTS.md、docs/current.md、决策记录，或发现文档与代码对不上、上下文开始漂移时使用；附带只读审计脚本定位问题。Use for repo context governance, AGENTS.md setup, docs drift audit.
license: MIT
metadata:
  version: "3.0.1"
---

# Repo Governance (仓库上下文治理)

目标是让任何新会话或新 Agent 都能用最少的阅读量拿到正确的项目上下文。不规定语言、框架或代码组织方式。

## 先审计

```bash
<本技能目录>/scripts/audit-context.py [--repo <dir>] [--report] [--only entry,current,budget] \
    [--max-entry-tokens 5000] [--max-current-tokens 2500] [--fail-on links,names,adr-index]
```

只读，默认运行全部检查，每个问题输出一行 `WARN <文件>: <原因>`，命中 `--fail-on` 类别时退出 1。`--only` 只运行指定类别；`--report` 按占用率列出预算文件及每会话必读基线，始终退出 0。检查项：入口文件是否缺失或超预算；`docs/current.md` 的下一步是否过多、是否长期未更新、是否超预算、是否堆积了带日期的条目；入口文件与 `current.md` 用反引号点名的路径和 `make` 目标是否存在；`.local/` 是否被 gitignore；Markdown 相对链接是否断裂；ADR 索引是否与各 ADR 状态行一致。根据结果决定要不要动文档，不要为了"齐全"而补文件。

预算由被审计仓库根目录的可选 `.repo-governance.json` 定义，例如 `{"budgets":{"AGENTS.md":5000,"docs/current.md":2500,"skills/*/SKILL.md":4000},"maxDatedItems":5,"maxNextActions":5,"staleDays":30}`。`budgets` 的键是相对仓库根的 glob，精确路径优先于通配（可给个别大文件单设“只减不增”的上限）；未配置时入口文件默认 5000 token，`docs/current.md` 默认 2500 token。命令行阈值覆盖配置，配置覆盖默认值；旧的 `--max-agents-lines` 与 `--max-current-kb` 显式传入时仍检查并提示弃用。估算公式为 CJK 字符数 + ceil(其余字符数 / 4)，只用于预算与比较，并非任何模型的精确计数。行数和 KB 对中英文、长短行失真：同样 8 KB，中文约 2700 token、英文约 2000 token；一行可能 10 字也可能 400 字。阈值属于仓库自身，应随仓库演进调整；先用 `--report` 看余量。

接进仓库门禁时用 `--fail-on` 只让确定性问题失败（如 `links,names,adr-index`），体积与过期类只打印；脚本在技能目录里，门禁找不到它时应跳过而不是失败，干净克隆与没装技能的机器照常通过。

## ADR 索引

```bash
<本技能目录>/scripts/adr-index.py [--repo <dir>] [--write]
```

决策目录（`docs/decision`、`docs/decisions` 或 `docs/adr`）的 `INDEX.md` 里用 `<!-- adr-index:start -->` 与 `<!-- adr-index:end -->` 包住表格后，表格由各 ADR 的一级标题与第一条"状态/Status"行生成：不带参数只核对（过期退出 1），`--write` 重写标记之间的内容，标记外的文字保持不变。改状态改 ADR 文件本身，索引不再手写，也就不会两处不一致。

## 两条原则

1. **代码事实优先**：文档和运行态配置、代码冲突时以后者为准，并顺手修正文档。文档只是缓存，过期的缓存比没有更糟。
2. **渐进式沉淀**：只有需要跨会话保留的信息才落盘，不预先铺空目录和占位文档；每多一个文件，接手者就多一份阅读和核对成本。

## 信息分层

| 位置 | 放什么 | 为什么放这里 |
| --- | --- | --- |
| `AGENTS.md`（或等价入口） | 项目目标、关键命令、验证入口、改动如何生效（重建、重启、迁移）、高风险禁区 | 每个会话都会自动加载，越短越不容易被忽略 |
| `docs/current.md` | 当前焦点、阻塞项、≤5 条下一步，以及尚未收口的状态（未验证、已提交未生效、已知风险） | 长任务的断点，每个会话都读；已完成、已上线的记录在提交信息里，完成即清理 |
| `docs/reference/` | 已验证的稳定架构事实 | 与规划混在一起会误导接手者，规划内容需标注 *Proposed* |
| `docs/decision/` | 影响深远的选型、备选方案与理由 | 代码能说明"是什么"，说明不了"为什么没选别的" |
| `docs/roadmap.md` | 长期方向与里程碑 | 与当前任务解耦，避免 `current.md` 膨胀 |
| `.local/`（整体 gitignore） | 运行产物、任务草稿 `.local/plan/` | 临时内容不进版本库；稳定结论在任务结束前回填到上面几层 |

小项目只需要 `AGENTS.md`；出现跨会话的长任务再加 `docs/current.md`；其余层按需出现。
