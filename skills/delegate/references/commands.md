# 命令与证据

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `reply <run> [消息] [--wait] [--fresh] [--sync\|--sync=agent\|--no-sync] [--allow <path>]` | 接着最新一轮会话在后台启动并立即返回；`--wait` 等到结论（此时可用 `--max`/`--progress`/`--full`）。写入 worktree 续做默认自动把本链净改动三方合并到当前源快照并更新工作树基线（保留 pool slot 的 `target/`，续做提示附「基线已同步到主干 \<短 hash\>」），有冲突时保持旧基线并在 stderr 列重叠文件；`--no-sync` 跳过自动同步，`--sync` 遇冲突直接拒绝，`--sync=agent` 留文本标记并在说明首行列冲突要求先解决，未清标记拒收（后续普通 reply 仍检查），二进制/符号链接/结构冲突仍拒绝。`--allow` 在已有保护范围内为本轮及后续续做开放路径并回显；`--fresh` 开新会话，消息须自足。验收与证据命令默认沿用，可分别覆盖或关闭；取消或隐藏已变更的验收命令时会告诉同事旧标准不再适用 |
| `patch <run> [-- <path>…]` | 打印保留的完整补丁，可按精确路径/目录前缀过滤，供 `git apply --3way` 恢复 |
| `diff [<run>] [--stat] [--total] [路径...]` | 以 `git diff` 输出该轮的改动；`--total` 为整段对话；终端下带颜色 |
| `apply [<run> …] [--dry-run] [--merge] [--verify\|--no-verify] [--keep] [--keep-commits\|--commit] [--json]` | 按给出的顺序将 worktree 改动合入源工作区，默认不提交并在完成时提示「改动未提交：N 个文件」与同事原提交标题，遇到失败即停并明报 `failedStep` 与已写入源工作区的 `writtenFiles`；验收没法沿用（`acceptStillValid` 不为 true）的任务在最后一次合并后、经 lane 重跑一次各自的验收（去重），`applyVerify: false` 关闭并改用事实措辞提示批量门禁命令、写命令则替换；始终列同事提交。`--keep-commits` 按序 cherry-pick，要求同事工作树改动已提交；`--commit` 仅暂存本次写入路径并按同事提交标题压成一条提交（源暂存区已有改动或三方合并含本地未提交改动时失败关闭）；收尾与 apply 的 `commitsDiffer` 列提交以外文件，默认 apply 始终取最终树。成功后按 `worktree.cleanOnApply`（默认 true）归档并删除 worktree（续做链等整条链都合入后再清），保留原 run 轻量记录与 applied.patch；`--keep` 或 `cleanOnApply: false` 保留 worktree。合入末行报告验收有效性与盲区。 |
| `busy <资源名>` | 有实际运行的匹配命令时逐行列 run、命令和持续秒数，退出 0；空闲无输出、退出 1 |
| `wait [<run>...\|--all] [--max <时长>] [--no-result] [--full] [--progress]` | 无参和带名字的 `wait <run>...` 默认同 `--any`：返回时列出上次 wait 之后所有进入终态或提问态的 run（首个给全文，其余给一行摘要加 `result` 查看命令）；`--all` / `--until-all` 等全部（单次最多 50 分钟，超时列仍在跑的 run）。人读末行给本会话剩余数和原样重跑命令，无运行任务时明确说明 |
| `status [<run>...] [--all] [--json] [--files]` | 默认首行计数（运行中/待合入/待回答/排队/待办等），只列待处理任务与仍有未完成待办的任务/已清理链（续做链折成末环一行如 `合入员（第 3 轮）`，路径清单只留计数），末行指向 `delegate history` 与归档占用；指定 `<run>` 展开路径与待办全文；读过 `result` 或 `apply` 记入 `meta`；`--all` 列全部轮次，`--json` 默认省去 `files`（加 `--files` 才带） |
| `note [<run> ["<文字>"\|--done [<序号>\|--all]]] [--all] [--json]` | 在 run 上追加带时间的合入后待办、查看或标完成；备注随整条续做链走，存在 `<runs>/notes.json`，`clean` 时有未完成待办仍保留直到 `--done` |
| `handoff [--out <路径>]` | 生成未清理、未合入任务的一页 Markdown 交接索引（默认写入 `.local/run/delegate/HANDOFF.md` 并打印路径）：名字、状态、`netBase` 短 sha、净改动、净 patch 新增 `TODO(master)` 计数、`migrations/` 文件名、下一步、每个 run 下的未完成待办、重叠修改文件及「合入后待办」汇总 |
| `history [关键词…] [--since <时长>] [--state <状态>] [--limit N] [--json]` | 查本仓库永久追加索引 `history.jsonl`（老的在上新的在下，每条一行指向 `show`）；关键词匹配名字、提示词与结论，中文支持单字加二元组 |
| `show [<run>] [--json]` | 第二层单 run 摘要：提示词前几行、确定性结论段、验收行、合入后源仓库同文件提交（现查 `git log`）、续做链与归档路径，末行指向 `result`/`patch`/归档原文；clean 后从归档读，归档过期说明「原文已过期，仅剩索引」 |
| `board [--path]` | 刷新并打印本工作区的自动委派看板（默认放在 history 目录旁 `BOARD.md`，可用仓库 `.delegate.json` 的 `board` 配置路径）；每次状态变化自动原子重写，顶行含整机在途与各节计数，写失败只记诊断不阻断委派 |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其 scope、进程组 |
| `clean <run>...\|--finished [--force]` | 删除已结束的任务；`--finished` 默认保留结果未读取的 |
| `lane [--label <文字>] [--measure] [--] <命令>` | 在整机重任务队列里执行命令；`--measure` 使用测量预约核并串行排队（一个参数按 shell 命令执行），退出码原样返回；不带命令时列出正在跑与排队的项 |

启动选项：`--tier scout|review|write`（用途，不写时只读推断 review、写入推断 write，明确提醒并标记；scout/review 自动只读；三档须配齐）、`--image <路径>`（可重复）、`--accept <命令>` / `--no-accept`（覆盖或关闭仓库默认验收）、`--hide-accept`、`--accept-timeout`（默认 `.delegate.json` 的 `acceptTimeout`，未设时 `10m`；`reply` 继承）、`--read-only`、`--shell`（仅 git 快照内只读调查，写文件拒收）、`--in-place`（只读读实时工作区；写入覆盖 worktree 默认并原地写入）、`--workdir`、`--timeout`（每次尝试，可选；没有默认值）、`--retries`（答复畸形时重跑次数，默认 1）、`--model <别名或已登记完整名>`（可与 --tier 同用，只在 models 别名表内精确/唯一子串匹配，固定模型不自动换）、`--thinking off|minimal|low|medium|high|xhigh`（独立长度，模型名不带后缀；reply 也可换模型、档位或长度）、`--allow-parallel-writes`、`--worktree`、`--in <run>`（只读，在已完成任务 worktree 的快照里审它的改动）、`--protect <路径>`（可重复；末尾 `/` 为目录；被改动即判 `rejected` 并带 `protectViolation`，不跑验收；reply 沿用；正文出现未放开的保护路径时启动回显与 status/wait 均提示补 `--allow`，不拦截）。`--name` 省略时取说明首分句生成最多 24 字的可读名称，唯一 ID 规则不变；reply 轮次命名不变。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

`start`/`run`/`reply` 另支持 `--evidence <命令>`、`--no-evidence` 与 `--evidence-timeout`（默认 30m）。reply 沿用上一轮证据命令与超时，可覆盖或关闭。`--model` 固定模型时仍显示所在档位，JSON `agentPinned: true`，不自动升档；旧 meta 缺字段时推断显示，不改写旧 meta。6.0 删除了 `--agent`；Codex 留下的旧记录 `reply` 时由 Pi 开新会话续做。

`start`/`run`/`reply` 支持 `--max-answer N`（正整数，reply 继承）：超出 1.5 倍时续接压缩一次；失败时只显示前后截断的 N 字及原文路径、字数、原因，`result` 仍读全文。原文另存 `result-original.md`，不耗返工预算。`start --share PATH` 可重复，repo 内在 worktree 同位置建链接，repo 外只写入任务说明，reply 继承。固定说明与默认参数见 [配置](configuration.md)。

## 证据命令

证据用于真实模型评测等慢且不确定的检查。验收通过后执行；未设验收时在正常答复后执行；验收失败或 protect 违规时跳过。在同事工作目录（隔离任务为其 worktree）通过 lane 排队，用验收相同的 env、原 PATH、独立进程组及 cgroup 回收，deny shim 不影响证据。拿到 lane 名额才开始计算 `--evidence-timeout`。

`start`/`run`/`reply` 可重复使用 `--accept-also <命令>`，按顺序以 `&&` 追加到生效验收；无基础命令则独立运行，与 `--no-accept` 冲突退出 2。

证据命令默认未设置。结果写入 meta 与 JSON 结论的可选 `evidence{exit,timedOut,seconds,tail,log}`：退出码、是否超时、执行秒数、日志末尾和完整日志位置。短行加“证据 通过/失败/超时”。任何证据失败只记诊断，永不改变任务 state、升档判断或命令退出码；它也不替代 `acceptStillValid` 的验收快照依据。

等待检查线：Claude Code 在未给 `--max` 时默认 50m 后打印每个运行任务的进度 glance 并退出 75；`--stream` 每 50m 打印一次并继续。`checkIn`、`DELEGATE_CHECK_IN` 可改时长，`0` 关闭。
