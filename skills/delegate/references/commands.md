# 命令与证据

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `reply <run> [消息] [--wait] [--fresh] [--sync]` | 接着最新一轮会话在后台启动并立即返回；`--wait` 等到结论（此时可用 `--max`/`--progress`/`--full`）。`--fresh` 开新会话，消息须自足；`--sync` 先同步主控后来改动，冲突时直接拒绝。验收与证据命令默认沿用，可分别覆盖或关闭；取消或隐藏已变更的验收命令时会告诉同事旧标准不再适用 |
| `diff [<run>] [--stat] [--total] [路径...]` | 以 `git diff` 输出该轮的改动；`--total` 为整段对话；终端下带颜色 |
| `apply [<run>] [--dry-run] [--merge] [--keep] [--keep-commits] [--json]` | 默认将 worktree 改动合入源工作区，不提交；始终列同事提交。`--keep-commits` 按序 cherry-pick，要求同事工作树改动已提交，冲突停在 git cherry-pick 状态。成功后删除 worktree，保留轻量 run 记录；`--keep` 保留完整记录。合入末行报告验收有效性与盲区。 |
| `busy <资源名>` | 有实际运行的匹配命令时逐行列 run、命令和持续秒数，退出 0；空闲无输出、退出 1 |
| `wait [<run>...\|--all] [--max <时长>] [--no-result] [--full] [--progress]` | 无参默认同 `--any`：交付已结束未读取的，否则等下一批结束即返回；`--until-all` 等全部，`--all` 仅控制范围。人读末行给本会话剩余数和原样重跑命令，无运行任务时明确说明 |
| `status [<run>...] [--json]` | 默认每任务一条自然语言短行；`--json` 保留完整原字段 |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其 scope、进程组 |
| `clean <run>...\|--finished [--force]` | 删除已结束的任务；`--finished` 默认保留结果未读取的 |
| `lane [--label <文字>] [--] <命令>` | 在整机重任务队列里执行命令（一个参数按 shell 命令执行），退出码原样返回；不带命令时列出正在跑与排队的项 |

启动选项：`--tier cheap|strong`（默认只读 cheap、写入 strong；便宜档失败且未改动时自动升档一次）、`--agent pi|codex`（直接指定，与 `--tier` 互斥，不升档）、`--image <路径>`（可重复）、`--accept <命令>` / `--no-accept`（覆盖或关闭仓库默认验收）、`--hide-accept`、`--accept-timeout`（默认 10m）、`--read-only`、`--in-place`（只读读实时工作区；写入覆盖 worktree 默认并原地写入）、`--workdir`、`--timeout`（每次尝试，pi 默认 25m，codex 默认 50m）、`--retries`（答复畸形时重跑次数，默认 1）、`--model`/`--thinking`/`--provider`（不指定时用各 CLI 自己的默认设置；Codex 的 `--thinking` 对应推理强度）、`--allow-parallel-writes`、`--worktree`、`--after <run>`（等上游以 delivered/answered 结束再执行，否则 `skipped`；等待期间 state 为 `waiting`、不占名额）、`--in <run>`（只读，在上游 worktree 的快照里审它的改动）、`--protect <路径>`（可重复；末尾 `/` 为目录；被改动即判 `rejected` 并带 `protectViolation`，不跑验收；reply 沿用）。`--name` 省略时取说明首个非空行生成最多 40 字的可读名称，唯一 ID 规则不变；reply 轮次命名不变。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

`start`/`run`/`reply` 另支持 `--evidence <命令>`、`--no-evidence` 与 `--evidence-timeout`（默认 30m）。reply 沿用上一轮证据命令与超时，可覆盖或关闭。`--agent` 固定同事时仍显示适配表默认档位（Pi cheap、Codex strong），JSON `agentPinned: true`；旧 meta 缺字段时按同一规则推断显示，不改写旧 meta，不自动升档。

`start`/`run`/`reply` 支持 `--max-answer N`（正整数，reply 继承）：超出 1.5 倍时续接压缩一次；失败时只显示前后截断的 N 字及原文路径、字数、原因，`result` 仍读全文。原文另存 `result-original.md`，不耗返工预算。`start --share PATH` 可重复，repo 内在 worktree 同位置建链接，repo 外只写入任务说明，reply 继承。固定说明与默认参数见 [配置](configuration.md)。

## 证据命令

证据用于真实模型评测等慢且不确定的检查。验收通过后执行；未设验收时在正常答复后执行；验收失败或 protect 违规时跳过。在同事工作目录（隔离任务为其 worktree）通过 lane 排队，用验收相同的 env、原 PATH、独立进程组及 cgroup 回收，deny shim 不影响证据。拿到 lane 名额才开始计算 `--evidence-timeout`。

`start`/`run`/`reply` 可重复使用 `--accept-also <命令>`，按顺序以 `&&` 追加到生效验收；无基础命令则独立运行，与 `--no-accept` 冲突退出 2。

证据命令默认未设置。结果写入 meta 与 JSON 结论的可选 `evidence{exit,timedOut,seconds,tail,log}`：退出码、是否超时、执行秒数、日志末尾和完整日志位置。短行加“证据 通过/失败/超时”。任何证据失败只记诊断，永不改变任务 state、升档判断或命令退出码；它也不替代 `acceptStillValid` 的验收快照依据。
