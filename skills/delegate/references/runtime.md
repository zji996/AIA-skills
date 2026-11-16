# 等待、lane 与后台回收

`XDG_STATE_HOME` 未设时使用系统约定的用户状态目录。

## 重任务队列（lane）

`status`/`wait` 默认只展示决策所需的状态、同事与档位、耗时、文件数与增删行、前三个目录、最近命令（运行中，最多 100 字符）、验收与下一步，文件路径相对仓库；默认 status 不做工作树快照或 diff（已结束任务用 summary，老记录缺 finalEntries 不补算），运行中的源差异用 supervisor 定期缓存；容量/源观察锁只非阻塞尝试，繁忙显示 busy 与缓存，不等待编译/apply。队列人数在锁空闲时轻量更新，不扫描整机进程。`wait` 仍随后附答复。脚本用 `--json` 读取原有完整结构，`start`/`run`/`reply`/`stop` 也支持此选项；`result`、`diff` 输出不变；apply 人读末行给验收提醒，`--json` 保留既有结论。JSON 的运行目录、worktree、结果位置仍保留可直接访问的绝对路径。

提问收尾仍回收进程，但把尚未开始的 lane 票按 label 保存原排队时间；回答后重跑同一 label 的作业先于较晚的排队者，已运行者不被抢占。浏览器测试不自动另开轻量 lane：浏览器、服务夹具与编译都可能吃内存，另开槽会隐性突破 maxHeavy/测量隔离；仍共用 heavy lane，由用户决定容量。

每任务有 `waiter.lock`，持有排他 flock 并记录等待者 PID；重复 `wait` 跳过已有活等待者覆盖的任务，全被覆盖时退出 `76`，通知仍给原等待者。`--any`、`--stream`（含后续加入的任务）、`--machine` 同样处理；等待者退出或死亡时内核释放锁，旧 PID 不阻止后续等待。

整机一条先进先出队列，同时放行 `DELEGATE_MAX_HEAVY` 个（默认 2，`0` 不限）：验收、证据、worktree `setup`、同事与主控用 `lane` 跑的检查都在这里排队。每个排队者在 `${XDG_STATE_HOME}/delegate/lane/` 下有一张按到达时间命名的票，持有其排他 flock 直到结束；等待者阻塞在前一张票的锁上，由内核在其结束或进程死亡时唤醒，不轮询，崩溃不留死锁。已在队列内的命令（带 `DELEGATE_LANE_HELD`）再调用 `lane` 直接执行，不会等自己。

排队时间不计时：验收的 `--accept-timeout` 与 `setup` 的超时从拿到名额开始算；同事自己用 `lane` 排队的时间记在 run 目录的 `lane-wait`，从它的 `--timeout` 中扣除。同事、每条 setup 与验收命令在独立进程组中运行；用户 systemd 可用时各自再进入独立 scope，结束或超时时按 cgroup 回收其派生进程（包括 `env -i`、`setsid` 后的进程）。无用户实例时自动沿用进程组与环境标记清理；`DELEGATE_CGROUP=0` 或 `PI_DELEGATE_CGROUP=0` 强制关闭 scope。supervisor 与主控的 `wait`、排队中的其他任务不进入这些 scope。

## Remote 执行与远端 lane

start/reply 保存当次生效配置（remoteOff 随 reply 继承，显式 DELEGATE_REMOTE 可覆盖）；默认 root 是远端 HOME 下 `delegate`、programs 是 `["cargo"]`、maxHeavy 为 4（0 不限）。root/profile 为不越界的相对路径，profile 相对 root，slice 可省。

本机与远端需 SSH、rsync，远端需 Linux 的 setsid、程序及其工具链；免交互认证与 profile 由用户准备。meta.env 与 forwardEnv 的本机现值透传，PATH/HOME/PWD/SHELL/TMPDIR、XDG 路径及 DELEGATE 内部标记不透传；远端工具链搜索路径在 profile 配置。profile 在执行程序的 shell 中 source；有 slice 时在 systemd service 的 payload shell 中 source，避免丢掉 profile 的导出变量。

remote 生效时，同事的 argv0 命中 programs 就转给 `_remote`；按名称的 PATH shim 路由，不拦截显式 `/path/to/cargo` 或自行重置 PATH。匹配 measure 的同事命令仍在本机测量，已有 agentDeny 仍可拒绝；其他命令先 agentDeny、再 remote、最后 resources，远端命令不登记本机资源。验收、evidence、setup、生成与合并后 verify 使用只含 remote 规则的 PATH（不应用同事 deny/measure/resources），整个 shell 步骤改占远端 lane；嵌套 `_remote` 通过 `DELEGATE_LANE_HELD` 不重复排队。主控 `lane <cmd>` 不变，仍本机；未实现 `lane --remote`。

`${XDG_STATE_HOME}/delegate/lane-remote/` 复用票 + flock 的 FIFO，宽度独立取 remote.maxHeavy；同事中途编译也排队，stderr 一行提示，等待记入 run 的 lane-wait，排队不计运行超时。`lane`/人读 `status` 列远端 running/queued 持有者；run JSON 的 remoteLane 含 holders 与 queued 人数。每个远端槽的上传/执行/下载再用本机 flock 串行，避免同事并行 bash 互删文件。

槽路径是 `<root>/<repo-key SHA1 前12位>/<worktree目录名>`，repo key 沿用本地 Git common-dir 身份，同仓库池槽保持固定目录与温热缓存。每次从工作树根双向 `rsync -a --delete --filter=':- .gitignore' --exclude=/.git --exclude=/target`，保留被忽略的 target/node_modules 等；额外使用 protect-args 并排除本工具瞬时 PID/取消文件。不做 dereference，worktree.link 符号链接原样传；远端目标缺失由 profile 处理。**不传 .git**（worktree 的 .git 指向本机）：需要 git 的构建命令会失败，这是已知限制。cwd 保持相对工作树根的位置；stdout/stderr 逐行把远端槽绝对路径改成本机路径，参数/环境逐项 shell quote，stdin 转给 SSH，完成后即使命令非零也回传源改动，原样返回程序退出码（包括 255）。

有 slice 时 `systemd-run --user --unit=delegate-<id> --slice=… --wait --pipe --collect --quiet --working-directory=… -E …` 包住命令；无 slice 用 setsid 进程组与唯一 PID 文件。SIGTERM/INT/HUP 写远端取消标记、停止进程组，有 slice 再 `systemctl --user stop`，随后结束本机 SSH；标记防止取消恰好早于远端启动的竞态。远端连接断开时也尝试清理。传输失败 stderr 一行 `delegate remote: ci unreachable (<原因>); remote mode does not fall back to local`，退出 75，**绝不回退本机**；网络完全断开时不能确认远端回收，恢复连通后须检查。

验收的传输故障在 summary/status 标为 infrastructureFailure，命令未执行另标 accept.notExecuted；程序自身退出 75 仍是执行失败。远端槽锁排队也从 shell 执行超时中扣除；未执行不产生 acceptStillValid=false（apply 记 acceptPending），apply 给重排与本地跑命令，重试不占返工预算。`--after` 等未执行的上游 reply 重试，不静默跳过；确实失败才 skipped，由 wait 报原因与 reply --fresh 重启命令。clean/apply 清理非池化独占工作树时顺带删远端槽；池槽保留。远端不可达只诊断，不阻止本机清理。

### 后台服务回收

5.19.0 起，以上回收阶段还检查项目脚本通过 `systemd-run --user` 启动、脱离任务进程组和 scope 的 service/scope；`clean` 和过期清理在删除 run/worktree 前也检查历史遗留。只查询用户实例，不按 unit 名前缀归属任务。以下任一条件足够：

- `Environment` 包含完整且相等的 `DELEGATE_RUN_DIR=<本轮 run 目录>`；复用进程回收的标记，支持 systemctl 的引号与 C 转义。项目脚本需用 `systemd-run --user --setenv=DELEGATE_RUN_DIR="$DELEGATE_RUN_DIR" ...` 显式传入；用户 manager 不会自动继承调用者环境。
- 隔离 worktree 中，`WorkingDirectory` 或 `ExecStart` 的可执行文件/argv 绝对路径落在本任务 worktree 内；按路径组件判断，拒绝 `..` 和指向树外的符号链接，不解析 `sh -c` 字符串里的路径。源仓库的同名前缀服务不会命中。

in-place 只使用环境标记；没有标记的 unit 保留，并在 `cleanup.diagnostics` 说明。worktree 缺失或被另一活任务共用时也只按标记判断。delegate 自建的 scope 继续走原 cgroup 回收，不重复停止/计数。

回收按可执行文件名豁免内置 sccache 与配置 cleanupKeepExecutables；含豁免进程的组/scope 逐 PID 终止其余进程，共享守护 unit 保留，回收提示包含可执行名。

先提交 `systemctl --user stop --no-block`，等待最多 3 秒；超时后 `systemctl --user kill --signal=SIGKILL`，再确认停止。每次 systemctl 调用最多 3 秒。缺少 systemctl、无 user bus 或回收失败只记诊断，不改任务 state 和退出码。

`cleanup` 保留 `terminated`（进程数）、`ports`、`commands`，另含 `systemdStopped`（本轮停止的不同 unit 数）、`units`（unit 名及 `matchedBy`/`killed`）、`diagnostics`。回收详情存于 `cleanup.json`，累计多阶段结果；短提示例如 `note: 任务结束时停止了 2 个 systemd 服务；答复中提到的服务/地址已不可用`。`clean` 的 removed 行仅计此次清理新停止的 unit。

同事的超时：到 `--timeout`（不含排队）时若有命令正在执行，或 120 秒内有事件，继续运行，最多到 `--timeout` 的 `1 + DELEGATE_TIMEOUT_GRACE/100` 倍（默认 1.1 倍）；结论里 `graceSeconds` 记下超出的秒数。
