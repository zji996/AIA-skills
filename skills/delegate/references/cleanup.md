# 清理与磁盘占用

## 容器回收

5.23 起移除 worktree 前按 compose label `com.docker.compose.project.working_dir` 回收该目录及其子目录的容器，目录已不存在时仍按记录路径匹配；仅前缀相同的兄弟目录不匹配。`docker rm -f -v` 删除匿名卷，保留具名卷。

`clean` 另扫描 delegate 缓存的 `worktrees/` 下 label 目录已不存在的旧容器。每次查询和删除累计限时 20 秒；Docker 不可用、无权限或超时只提示，不影响清理，同次命令失败后跳过后续 Docker 调用，避免反复等待。

`clean --json` 每个删除结果含 `containersRemoved` 与 `diagnostic`（无诊断为 null），末尾 `orphanCleanup` 含同样字段；文本报告对应数量。

## 磁盘提示

写入 run 结束、验收与证据收尾后用限时 `du -sb` 测一次，占用字节存入 `meta.worktreeBytes`，状态 JSON 同名输出；已有值复用，失败不记录，`status`/`wait` 不测量旧任务。

本仓库已结束、未清理且有记录的 worktree 按路径去重（运行中的续接目录不计），合计达到默认 20 GiB 时末尾打印一行总量、最大三项与下一步。`DELEGATE_WORKTREE_WARN_GIB` 调整阈值，`0` 关闭。

JSON 对应追加独立 `worktreeDisk` 行，含 `totalBytes`、`thresholdBytes`、`largest`（最多三项，每项 `run/name/bytes`）、`next`，未达阈值或关闭时不输出该行。例：

```json
{"worktreeDisk":{"totalBytes":35433480192,"thresholdBytes":21474836480,"largest":[{"run":"r1","name":"impl","bytes":12884901888}],"next":"已确认合入后：delegate clean <name> --force"}}
```

改写后手动合入并提交的任务立即 `clean <name> --force`；检测只认完全包含，改写后可能仍显示部分包含。

## 源仓库的构建缓存

5.24 起，写入 run 收尾在 worktree 测量之后限时测一次源仓库构建目录（各路径共用 20 秒），默认根有 `Cargo.toml` 时量现存的 `target`。用户 config.json 与仓库 `.delegate.json` 可用 `sourceBuild.paths` 替换默认探测，`[]` 关闭；路径和合并规则见 [配置](configuration.md)。原地写入也测，只读不测；不存在的目录跳过，同一实际目录去重。空列表或无现存目录缓存零值，测量或缓存失败省略且不改 run 结论。

meta 缓存 `sourceBuildBytes`、`sourceBuildPaths:[{path,bytes}]`、`sourceBuildMeasuredAt`（UTC）、`sourceBuildMeasuredNs`（排序用纳秒字符串）；另有启动配置 `sourceBuild` 与源根 `sourceBuildRoot`。status/wait 不现量，同源仓库取最近一次成功缓存，不累加各 run；达到 `DELEGATE_SOURCE_BUILD_WARN_GIB`（默认 60 GiB，0 关闭，支持小数）时末尾自动提示，与 worktreeDisk 独立，可同时出现。例：

```text
提示：源仓库构建目录共 60.0 GiB；各路径：target 60.0 GiB；先量再删：见 references/cleanup.md 的“源仓库的构建缓存”
```

JSON 追加独立 `sourceBuildDisk` 行，含 `totalBytes`、`thresholdBytes`、`paths`（每项 `path/bytes`）、`measuredAt`、`next`；未达阈值或关闭时省略。提示只基于缓存，不代表当前大小；delegate 只回收自己的 worktree，不删除源构建目录。验收、门禁和主控的构建缓存达到阈值（或磁盘告警）时按下面做，先量再删：

```bash
du -sh target/debug/* | sort -rh | head     # Rust 示例；其他工具链换成各自的构建目录
```

| 对象 | 做法 | 理由 |
|---|---|---|
| 增量编译目录（`target/*/incremental`） | 整个删 | 每个 crate 每种配置一份会话目录，只增不减；删后只是首轮构建变慢 |
| 工作区自有包的旧产物（`deps/` 下同名不同哈希的库与测试二进制） | 只删早于最近一次全量构建的 | 每次改源码留下一份新哈希的副本，旧的不会再被读取 |
| 第三方依赖的产物 | 不动 | 仍然有效；删了要整棵依赖树重编，写入量反而更大 |
| 编译缓存（sccache 等） | 不动 | 清理后的重建靠它 |

```bash
rm -rf target/debug/incremental
find target/debug/deps -maxdepth 1 -type f -mtime +1 \( -name 'myapp*' -o -name 'libmyapp*' \) -delete
```

筛选条件用“名字属于工作区 + 修改时间早于最近一次全量构建”。两种直觉上的条件筛不对：按访问时间，relatime 挂载下一周内几乎每个文件都被读过，筛出零个；“N 天前的一律删”，落进去的多是仍有效的第三方库，真正占地的是最近几天反复重编的自有产物。

删之前确认没有构建在跑；已启动的服务进程不受影响，缺失的产物下次构建会重建。

预防：在配置的 `env` 里给同事与验收设 `CARGO_INCREMENTAL=0`，增量目录就只由主控自己的构建产生。
