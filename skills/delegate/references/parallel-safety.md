# 并行写入的合入与恢复

## 合入前后

- `apply`（含 `--merge`、`--keep-commits`、`--dry-run`）先检查本次涉及路径的源文件。行首 `<<<<<<< ` 或 `>>>>>>> ` 表示尚有冲突：列出路径和标记标签（含留下标记的 run），整个 run 不写入源文件；不相关路径仍能合入。同源 apply 自动串行，避免检查与写入交错。
- 新写出的冲突仍返回非零，保留 `conflict-markers` 动作与醒目的 `applied with conflict markers` 结论；`summary.conflictMarkers` 保存待解决路径，`status` 在标记清除前显示「已合入·有冲突待解决」。先解决再合下一路，不能在标记上继续叠三方合并。
- 成功后释放 worktree / 池槽，但 run 原目录保留 `meta.json`、`result.md`、`summary.json`、`applied.patch` 等轻量记录。`applied.patch` 是该轮相对基线的完整 diff（含大文本与二进制补丁；超出快照范围的大二进制只列清单），不是合并后的源树 diff。

```bash
D=<本技能目录>/bin/delegate
$D patch impl > /tmp/impl.patch
$D patch impl -- src/module.rs > /tmp/module.patch  # 精确路径或目录前缀，可重复
# 先在目标仓库核对基线与工作区，再恢复：
git apply --3way /tmp/module.patch
$D clean impl                                   # 确认不再需要后删除
```

记录默认保留 7 天（`DELEGATE_KEEP_DAYS`，0 禁用过期）；`clean --finished` 同样可清。补丁独立于 Git 对象和已释放的工作树；`--keep` 另外保留工作树。

## 工作目录与只读调查

写入提示固定附「只在当前工作目录内改文件；提示里出现的其他绝对路径只读」。正文含源仓库绝对路径时启动给 note，不拦截。`meta.sourceStatusBaseline` 保存启动时 porcelain 路径/内容/索引指纹；运行中定期观察，`status` 显示 supervisor 定期缓存的 `sourceWorkspaceChanges`（非实时刷新）。结束时若自己的工作树零改动而源出现新增差异，判 `rejected`，显示「写到了源仓库」及数量/路径，JSON `sourceWriteViolation`，不再算提问。

delegate 自己的 apply 登记已写路径的最终指纹，排除其他路合入带来的差异；同路径后来再次被编辑仍能发现。它不能区分主控手改与 Agent 越界写入，属于误操作检测而非权限沙箱；比较范围遵循 Git status（不含忽略文件，也排除允许存日志的源 `.local/`），改后恢复原样不算最终差异。

同事的非交互 Bash 通过 `BASH_ENV`/DEBUG 在命令执行前检查：cd 进入源树不拦，cat/rg/grep/head/tail/ls、普通 sed/find、git status/diff/log 等读取放行；常见直接输出重定向到源树、在源树执行写命令或可能写入的未知 helper 则退出 77，仅一行指出拒绝的写入路径与本轮 worktree。`cp`/`rsync`/`install`/`ln` 只按目标判定（末个非选项参数，或 `-t`/`--target-directory` 指定目录；rsync 的 `-t` 仍是保留时间），允许从源树复制/链接到 worktree，反向或源树内复制仍拒绝。源 `.local/` 的明确日志输出与简单文件操作允许；本轮 worktree 的读写不受影响。拒绝发生在 shell 执行重定向之前；验收、生成器、主控 lane 不注入。

这是防误操作的字面启发式，不是 shell 解析器或权限沙箱：不可靠识别变量/命令替换形成的输出路径、复杂引号/空格、嵌套解释器、脚本内部写入、外部服务写入；非 Bash、`bash -p`/移除 BASH_ENV/重置 trap 可绕开，读取命令的特殊写入选项也不穷举。未知 helper 在源目录保守拒绝，应转到 worktree 执行；不能保证所有读取变体都不误报。事后源差异检查仍保留。

`start --tier scout|review --model <别名> --shell "查日志 / 跑只读 SELECT"`：tier 决定只读用途，model 只换模型（write 档也可同时指定）；只读默认仅文件读取/搜索工具，`--shell` 增加 Pi `bash`。也可 `--read-only --shell`。shell 调查必须在 git worktree 快照内（拒绝非 git 或 `--in-place`），不是源目录；结束时工作树或源有差异就判 `rejected`、显示「只读任务产生了写入」，始终禁止 apply。普通 reply 继承 shell；外部服务/数据库写入不在文件核对范围，查询必须自行保证只读。

## 等全部交付与生成器

`wait --all r1 r2 …` / `wait --until-all r1 r2 …` 等选中任务全部离开运行/排队态，旧交付不会提前唤醒；单次最多 50 分钟（`--max` 可缩短），超时返回 75 并列仍在跑的 run。无名字的 `--all` 还选本仓库跨会话任务；已有其他等待者覆盖时返回 76，不表示全部完成。默认 wait 保持任一结束即返回，人读首行提示选中列表里的已交付 run。

配置了 `generated.inputs` 时，仅本次改动命中输入才收尾复跑受保护生成物；`--protect` 重叠本身不触发。apply 仍仅对实际合并输入运行生成器；无输入命中时保留 `not regenerated: no merged path under generated.inputs` 的诊断（输出以列对齐）。未配置 inputs 则沿用有合并就生成；上次生成失败的 pending 重试仍执行。
