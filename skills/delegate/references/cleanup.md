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
