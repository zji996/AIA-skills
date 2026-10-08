# 等待、lane 与后台回收

`XDG_STATE_HOME` 未设时使用系统约定的用户状态目录。

## 重任务队列（lane）

`status`/`wait` 默认只展示决策所需的状态、同事与档位、耗时、文件数与增删行、前三个目录、最近命令（运行中，最多 100 字符）、验收与下一步，文件路径相对仓库；`wait` 仍随后附答复。脚本用 `--json` 读取原有完整结构，`start`/`run`/`reply`/`stop` 也支持此选项；`result`、`diff` 输出不变；apply 人读末行给验收提醒，`--json` 保留既有结论。JSON 的运行目录、worktree、结果位置仍保留可直接访问的绝对路径。

每任务有 `waiter.lock`，持有排他 flock 并记录等待者 PID；重复 `wait` 跳过已有活等待者覆盖的任务，全被覆盖时退出 `76`，通知仍给原等待者。`--any`、`--stream`（含后续加入的任务）、`--machine` 同样处理；等待者退出或死亡时内核释放锁，旧 PID 不阻止后续等待。

整机一条先进先出队列，同时放行 `DELEGATE_MAX_HEAVY` 个（默认 2，`0` 不限）：验收、证据、worktree `setup`、同事与主控用 `lane` 跑的检查都在这里排队。每个排队者在 `${XDG_STATE_HOME}/delegate/lane/` 下有一张按到达时间命名的票，持有其排他 flock 直到结束；等待者阻塞在前一张票的锁上，由内核在其结束或进程死亡时唤醒，不轮询，崩溃不留死锁。已在队列内的命令（带 `DELEGATE_LANE_HELD`）再调用 `lane` 直接执行，不会等自己。

排队时间不计时：验收的 `--accept-timeout` 与 `setup` 的超时从拿到名额开始算；同事自己用 `lane` 排队的时间记在 run 目录的 `lane-wait`，从它的 `--timeout` 中扣除。同事、每条 setup 与验收命令在独立进程组中运行；用户 systemd 可用时各自再进入独立 scope，结束或超时时按 cgroup 回收其派生进程（包括 `env -i`、`setsid` 后的进程）。无用户实例时自动沿用进程组与环境标记清理；`DELEGATE_CGROUP=0` 或 `PI_DELEGATE_CGROUP=0` 强制关闭 scope。supervisor 与主控的 `wait`、排队中的其他任务不进入这些 scope。

### 后台服务回收

5.19.0 起，以上回收阶段还检查项目脚本通过 `systemd-run --user` 启动、脱离任务进程组和 scope 的 service/scope；`clean` 和过期清理在删除 run/worktree 前也检查历史遗留。只查询用户实例，不按 unit 名前缀归属任务。以下任一条件足够：

- `Environment` 包含完整且相等的 `DELEGATE_RUN_DIR=<本轮 run 目录>`；复用进程回收的标记，支持 systemctl 的引号与 C 转义。项目脚本需用 `systemd-run --user --setenv=DELEGATE_RUN_DIR="$DELEGATE_RUN_DIR" ...` 显式传入；用户 manager 不会自动继承调用者环境。
- 隔离 worktree 中，`WorkingDirectory` 或 `ExecStart` 的可执行文件/argv 绝对路径落在本任务 worktree 内；按路径组件判断，拒绝 `..` 和指向树外的符号链接，不解析 `sh -c` 字符串里的路径。源仓库的同名前缀服务不会命中。

in-place 只使用环境标记；没有标记的 unit 保留，并在 `cleanup.diagnostics` 说明。worktree 缺失或被另一活任务共用时也只按标记判断。delegate 自建的 scope 继续走原 cgroup 回收，不重复停止/计数。

回收按可执行文件名豁免内置 sccache 与配置 cleanupKeepExecutables；含豁免进程的组/scope 逐 PID 终止其余进程，共享守护 unit 保留，回收提示包含可执行名。

先提交 `systemctl --user stop --no-block`，等待最多 3 秒；超时后 `systemctl --user kill --signal=SIGKILL`，再确认停止。每次 systemctl 调用最多 3 秒。缺少 systemctl、无 user bus 或回收失败只记诊断，不改任务 state 和退出码。

`cleanup` 保留 `terminated`（进程数）、`ports`、`commands`，另含 `systemdStopped`（本轮停止的不同 unit 数）、`units`（unit 名及 `matchedBy`/`killed`）、`diagnostics`。回收详情存于 `cleanup.json`，累计多阶段结果；短提示例如 `note: 任务结束时停止了 2 个 systemd 服务；答复中提到的服务/地址已不可用`。`clean` 的 removed 行仅计此次清理新停止的 unit。

同事的超时：到 `--timeout`（不含排队）时若有命令正在执行，或 120 秒内有事件，继续运行，最多到 `--timeout` 的 `1 + DELEGATE_TIMEOUT_GRACE/100` 倍（默认 1.1 倍）；结论里 `graceSeconds` 记下超出的秒数。
