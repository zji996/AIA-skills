# AGENTS.md

AIA-skills 是一个原子化 AI Agent 技能仓库。

## 维护原则

1. **原子性边界**：每个技能目录保持单一职责，严禁将通用的常识性编码规范混入已有技能中。
2. **能力优先**：优先提供可执行能力（脚本/工具）和清晰的触发条件；规则只保留脚本无法保证、需要模型判断的点，并写明原因。
3. **可路由的描述**：`description` 写清“做什么”和“什么时候用”，同时包含中文与英文关键词；`SKILL.md` 中引用本技能文件使用 `<本技能目录>/...` 或相对链接，不写依赖仓库根目录的路径。
4. **安装边界**：技能不应被自身调度的 Agent 加载时，在 frontmatter `metadata.exclude-agents` 中声明（如 `pi`），由 `scripts/install.sh` 避开对应目录。
5. **改动验证**：任何结构、文件或脚本变动后，必须运行 `./scripts/check.sh` 与 `python3 -m unittest discover -s tests` 确保无破损引用或格式错误。
6. **同步更新**：新增或修改技能时，务必在 `README.md` 中同步能力索引与描述，在 `evals/` 中维护触发示例，按语义化版本更新 `metadata.version` 并记入 `CHANGELOG.md`。
