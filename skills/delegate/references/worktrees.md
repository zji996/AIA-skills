# 改动快照、worktree 与合并

`XDG_CACHE_HOME` 未设时使用系统约定的用户缓存目录。

## 改动清单

在 git 仓库中，启动时和同事结束后（验收命令之前）各把整个工作区记为一个 tree 对象：借用真实 index 的副本执行 `git add -A` 与 `write-tree`，真实 index、分支和 stash 都不动；被忽略的文件不计，大于 `DELEGATE_SNAPSHOT_MAX_BYTES`（默认 2 MiB）的未跟踪文件只比较大小与修改时间。两次快照之差就是 `files`、`changes`（文件数与 +/- 行数）、`changes.json` 与 `changes.patch`，因此 shell 或脚本改的文件也会列出，改了又改回的不列，运行前已有的脏改动不算。原地运行时，别人在同一时间对仓库的改动也会被计入；需要干净归属时用 `--worktree`。子模块只作为一个条目出现：其检出中的已跟踪改动、未跟踪文件或提交变化都记为 `submodule contents`。快照失败时（例如 git 出错），结论带 `warning`，改动清单显示 unknown，只读任务也因此无法核验。非 git 目录只能根据编辑事件列出 `files`。

写入任务有快照改动时，`--json` 的 `shape` 补充改动形状：`dirs` 按路径前两级目录汇总增删行；`largest` 列出改后总行数最多的文本文件；`config` 列出被改动的依赖清单、锁文件、构建与 CI 配置；`removed` 列出删除文件。列表默认各最多 5 项，`DELEGATE_SHAPE_LIMIT` 可设为 1–20；超出时 `dirsMore`、`largestMore`、`configMore`、`removedMore` 记录未显示数量。`changes` 仍是整体文件数与增删行数，完整逐文件信息在 `changes.json`。JSON 模式沿用改动清单与 `shape` 小节；只读或无改动任务省略。

## worktree

`--worktree` 在 `${XDG_CACHE_HOME}/delegate/worktrees/<仓库名>-<run>` 建立 detached worktree（放在仓库外，免得测试、lint、文件监听扫到），检出的是启动时快照的提交（`commit-tree`，父提交为 HEAD，只被该 worktree 引用），所以同事看到的正是你当前的工作区（含未提交与未忽略的未跟踪文件），`git diff HEAD` 只显示它自己的改动；没有提交的新仓库也可以用。`--workdir` 为子目录时，同事在 worktree 的对应子目录工作。supervisor 在同事启动前按仓库根 `.delegate.json` 准备 worktree；其 `generated` 配置用于 `apply` 后重建生成文件（见 `docs/delegate-spec.md` §6.4）：

```json
{"accept": "make check", "generated": {"paths": ["src/generated/"], "command": "make generate"},
 "worktree": {"copy": [".env", ".local/scan"], "link": ["models/weights"],
              "setup": ["pnpm install --offline --frozen-lockfile"], "writeSetup": ["uv sync --frozen --offline"]}}
```

- `copy`：仓库根相对路径；被 git 忽略的文件或目录（如 `.local/scan`）也会递归复制，改动不影响原仓库。源不存在或为空时在 stderr 和 `warnings` 提示。
- `link`：大而只读的被忽略目录，建符号链接，写入会落到原仓库。
- `setup`：依次在 worktree 根执行，输出写入 `setup.log`，任一失败即判 `failed`（`DELEGATE_SETUP_TIMEOUT`，默认 10m）。依赖用包管理器从本机缓存重建：pnpm 与 uv 以硬链接安装，几乎不额外占盘，但文件多时仍慢（含 torch 的数 GB venv 约 1 分钟）；不要 link `node_modules`、`.venv`，其中的可编辑安装指向原仓库源码。
- `writeSetup`：同 `setup`，只在写入任务里、`setup` 之后执行；只读任务跳过。
- 这三类路径不计入改动。子模块在新 worktree 里是空目录：只读使用时写进 `link`（会替换空目录），需要独立修改时在 `setup` 里初始化。
- 顶层 `accept` 是写入任务的默认验收命令；`--accept-also` 追加任务测试，`--accept` 覆盖，`--no-accept` 关闭，只读任务不使用默认。
- `generated.paths` 的匹配语义同 `--protect`；改动清单与 diff 仍列出这些文件，`apply` 跳过其合并，在合并其他文件后由 lane 在源仓库根执行 `sh -c` 的 `command`，日志写入 `generate.log`；可选 `inputs`（同样的匹配语义）限定只有合并路径命中时才生成，省掉纯文档改动的排队；`--dry-run` 只报告动作。生成命令失败时退出 1，已合并文件保留（§6.4）。
- `--protect`/`--protect-reason` 与生成路径重叠时，启动 stderr 提示生成命令例外并附在同事的任务说明里；该命令与路径保存到 meta，reply/fresh 沿用。收尾在 lane 中于 worktree 根复跑，比较生成路径的内容、权限和符号链接（含忽略文件）；一致则允许，漂移报 `protectViolation` 与“生成物被手改”，生成失败判 rejected，日志在 `protect-generate.log`。
- 验收证据将托管 copy/link 路径明确排除，`accept.excluded` 与 apply 结论的 `excluded` 列出仓库相对路径；这些路径上的 skip-worktree 不算缺口。未托管 gitlink 的目录缺失或为空、且指针等于 HEAD 时，其未初始化内容也列为范围外，但指针仍保留在证据树中。源子模块初始化后重新核验：干净且同指针可复用，脏内容、索引标记、非空未初始化目录、变更的未初始化指针或读取失败仍判证据不完整。

`--read-only` 在 git 仓库里默认也用这样的 worktree（`--in-place` 除外），只作为供阅读的快照：主控同时的编辑既不影响它读到的内容，也不会被算成它的改动；它违规写入的文件留在 worktree 里，记为 `readOnlyViolation`，不能 `apply`。两位同事在其中都有全部工具，`setup` 照常执行（它们可能跑测试）；只有无法隔离时（非 git 或 `--in-place`），只读 Pi 才只保留读文件、搜索、列目录。

worktree 由对话共享，`clean` 删除最后一个使用它的 run 时执行 `git worktree remove`，过期清理同理；`agent-handoff` 会列出尚未 `apply` 的写入型 worktree。

成功 `apply` 默认回收 worktree，轻量保留 run 的 `meta.json`、任务说明、最终答复与摘要 7 天（`DELEGATE_KEEP_DAYS`，0 禁止自动过期）；`clean <run>` 与 `clean --finished` 可提前删除。普通 `reply` 会从当前源分支新建 worktree，将原任务说明、上次答复和追加说明交给新会话；`reply --fresh` 仍只使用本轮说明。共享输入沿用原路径；repo 内路径必须在新 worktree 同位置不存在，否则报错。共享链接只是协作约定，并非文件系统写保护。

写入任务未 apply 时，`status`/`wait`/`clean` 仅检查累计改动清单中的路径，按类型、大小和哈希比较源工作树与同事结束时的最终内容，执行位也须一致，删除路径须在源中不存在；不要求提交进 HEAD。全部包含时写 `.applied` 并记 `appliedBy: "detected"`，短行“已合入（主干已含改动）”，`next` 改为清理建议，`clean --finished` 按已合入处理。仅部分包含或读取失败时视为未检测到，沿用原 apply 建议；检测不代表验收已在源中复跑。

同仓库串行 `apply`；慢合并可后台执行。stderr 实时报告检查、生成器排队（人数与前序任务）、生成日志绝对路径和执行超时；排队不算生成超时。生成失败留 `.generate-pending`，再次 apply 即使普通文件已合入也重跑生成。编号前缀冲突只警告，不改号或退出码；末尾 `operation: apply` JSON 带 `numberedPrefixConflicts` 和验收复用的原因。`acceptStillValid` 只在验收前后历史全树与最终源全树有完整相同证据时为 true，不完整时省略；忽略文件、环境、数据库、Git 历史另行判断，后续改动/reply 不沿用旧 true。`.delegate.json` 的 `applyVerify`（`true` 用顶层 `accept`，或直接写命令）开启合并后验收：`acceptStillValid` 不为 true 时经 lane 在源工作目录运行，结论带 `verify`，失败退出 1（合并已写入）；`--verify` 强制、`--no-verify` 跳过。
