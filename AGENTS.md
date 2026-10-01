# AGENTS.md

AIA-skills 是一个原子化 AI Agent 技能仓库。

## 维护原则

1. **原子性边界**：每个技能目录保持单一职责，严禁将通用的常识性编码规范混入已有技能中。
2. **能力优先**：优先提供可执行能力（脚本/工具）和清晰的触发条件；规则只保留脚本无法保证、需要模型判断的点，并写明原因。
3. **可路由的描述**：`description` 写清“做什么”和“什么时候用”，同时包含中文与英文关键词；`SKILL.md` 中引用本技能文件使用 `<本技能目录>/...` 或相对链接，不写依赖仓库根目录的路径。
4. **安装边界**：技能不应被自身调度的 Agent 加载时，在 frontmatter `metadata.exclude-agents` 中声明（如 `pi`），由 `scripts/install.sh` 避开对应目录。
5. **改动验证**：任何结构、文件或脚本变动后，必须运行 `./scripts/verify.sh`（安装本仓库的 pre-push hook 后推送会自动跑）；它执行 `check.sh`、单元测试，以及有 cargo 时的 `cargo clippy -- -D warnings`（delegate 用例会自行从源码构建被测二进制，不必先重建 `bin/`）。带二进制的技能发版用 `scripts/release-binary.sh build|verify|publish <技能>`，`bin.sha256` 随版本号一起提交；二进制以 GitHub Release 为主要下载源，Forgejo 可选。
6. **同步更新**：新增或修改技能时，务必在 `README.md` 中同步能力索引与描述，在 `evals/` 中维护触发示例，按语义化版本更新 `metadata.version` 并记入 `CHANGELOG.md`。
7. **经验沉淀**：能改成行为的经验优先改脚本或默认值。
   需要 Agent 知道的，在 `SKILL.md` 写一句规则；超预算先删除已由脚本保证或已过时的句子。
   起因与过程写进该技能的实战记录，每条不超过 6 行；问题被脚本解决后压成“已解决”表的一行（版本号加一句话），细节留在 git 历史。
   长流程放在该技能的 `references/*.md` 按需读取，`SKILL.md` 只留一行指向。
   预算按“合理膨胀放宽、不合理收紧”定：内容都有用就调到实测加少量余量，否则先删；规范新增内容要替换旧内容而不是追加。CHANGELOG 每条一两句，不复述实战记录。
