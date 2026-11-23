# 用户与仓库配置、检查拦截

`XDG_CONFIG_HOME` 未设时使用系统约定的用户配置目录。

## Remote：重命令转到构建机

SSH host 只认用户配置；仓库 `remote.host` 始终报错。

```json
{"remote":{"host":"ci","root":"build/delegate","programs":["cargo"],"maxHeavy":4,
 "profile":"env.sh","slice":"build.slice","forwardEnv":["RUST_LOG"]}}
```

无 host 即关闭。仓库可逐字段覆盖（数组整体替换），但不能设置 host；`remote: false` 关闭本仓库，`DELEGATE_REMOTE=0` 关闭且随 reply 继承，显式变量可覆盖。`config`/`--help` 展示生效值（含主机），`remote check` 一行检测 SSH/rsync/程序版本，失败 75。

环境、profile、依赖与限制见 [runtime.md](runtime.md)。

## 用户与仓库配置

用户配置 `${XDG_CONFIG_HOME}/delegate/config.json` 支持 `remote`、`agentDeny`、`measure`、`measureCpus`、`env`、`maxRework`、默认 `accept`/`acceptTimeout`/`evidence`、`maxActive`、`maxHeavy`、`heavyJobs`、`repoMaxActive`、`models`/`tiers`/`thinking`、`sourceBuild.paths`、`standing`、`defaults`、`resultChars`、`cleanupKeepExecutables`、`agy`。仓库根 `.delegate.json` 使用相同字段并保存仓库事实。

`measure` 是 argv 前缀数组，例如 `{"measure":[["cargo","xtask","bench-concurrency"]]}`；仓库配置覆盖用户配置。`measureCpus` 是预约核列表（如 `"8-11"`），未设置时取最后 `max(2, nproc/3)` 个在线逻辑核。匹配的同事、验收、证据和 lane 命令自动排队到机器级测量预约；预约开始时在跑的 lane 命令会让出预约核，预约期间普通 lane 命令使用其余核。

- 标量：仓库 > 用户 > 内置默认；`DELEGATE_MAX_ACTIVE`/`MAX_HEAVY`/`HEAVY_JOBS`/`REPO_MAX_ACTIVE` 最高，`0` 不限（`heavyJobs` 须 ≥1）。整机/仓库默认 32/0；**编译并发按线程自动推导与共享令牌池**：可用线程取 `available_parallelism()`、向上查找的 cgroup v2 `cpu.max`（向上取整）与 `nproc` 最小值，`budget = max(1, 线程 − 2)`；`maxHeavy` 默认 `max(2, budget / 12)`（内存/IO 保护）；CPU 由 `<state>/jobserver.fifo`（目标令牌 `budget − maxHeavy`）管理，每条 `lane` 命令由进程内线程维护私有代理 FIFO（注入 `MAKEFLAGS`/`CARGO_MAKEFLAGS` 与 `CARGO_BUILD_JOBS`，覆盖宿主及 `env.CARGO_BUILD_JOBS`），按资历权重 `1, 1/2, 1/4…`（每条保底 2）动态公平分配——每条最多借 `份额 − 1`（另有 cargo 自带 1 个隐含令牌），用不满的份额让给他人，单条结束自动升至满预算，进程被杀或 `lane`/`status` 时自动校正补回；显式上限用 `heavyJobs`（`lane`/`status` 标「显式」）；远端不做代理与 FIFO，探测一次并缓存远端线程按 `budget / remote.maxHeavy` 注入 `CARGO_BUILD_JOBS`（失败回退配置并写诊断）。**未覆盖项**：`cargo test` 测试线程、链接器（如 `mold`）线程不在令牌池内。写入默认 `accept`/`acceptTimeout`/`evidence` 仓库优先、CLI 覆盖或关闭；只读不继承。
- 模型选择分为 `tiers.scout/review/write`、`models` 别名与 `thinking`；models/tiers 按键合并，详见 [用途、模型与思维长度](model-routing.md)。
- 任务测试用可重复的 `--accept-also COMMAND` 以 `&&` 追加到生效 `--accept` 或默认命令（与 `--no-accept` 冲突退出 2）；`--accept` 替换默认命令时 start 提示原命令。
- `env` 按键合并（仓库优先）；`agentDeny` 按 `(argv, exact)` 去重（缺省 `exact: false`），仓库同键覆盖 hint，仓库可用 `allow: true` 撤销完全匹配的用户规则（用户级禁用 `allow`）。
- `sourceBuild` 整对象按仓库 > 用户 > 默认覆盖（`paths: []` 关闭），相对路径独立校验越界与外部符号链接（退出 2）；仅写入收尾测一次，阈值 `DELEGATE_SOURCE_BUILD_WARN_GIB`（默认 60 GiB，0 关闭），见 [清理](cleanup.md)。
- `standing` 支持字符串、数组或 `{"all","write","readOnly"}`，按字段仓库覆盖用户，附在任务末尾并列生效 deny；reply 沿用会话，`--fresh` 重读。
- `defaults` 按字段仓库覆盖用户，含 `worktree`、`keepCommits`、`protect`/`acceptAlso`、`timeout`、`evidence`；CLI 覆盖默认。`--allow <path>` 在保护范围内按次放行（reply/fresh 继承），正文提受保护路径仅提示补 `--allow`；`worktree.cleanOnApply`（默认 true）控制 apply 后清理。
- `resources` 按资源名仓库覆盖用户（前缀匹配同 `agentDeny`），只登记占用不加锁，`busy` 按 `/proc` 剔除已退出进程。
- `acceptBlind`（glob 数组，仓库覆盖用户）：交付命中仍可交付，但短行、JSON 与 apply 末行提示最多 5 个文件及余数。
- `resultChars`（正整数，默认 20000，`DELEGATE_RESULT_CHARS` 优先）：答复超 1.25 倍才截断显示头尾；`--max-answer N` 超 1.5 倍自动续接压缩，不计返工。
- `cleanupKeepExecutables` 追加去重（内置 `sccache`），按可执行文件名豁免回收，不能配路径。
- `worktree`、`generated`、`applyVerify`、`board`、`mergeAccept`、`autoMerge`、`radar` 只认仓库；用户级忽略并 stderr 提示。
- 两份文件独立校验，错指文件与字段退出 2；meta/summary/`--json` 记 `configSources`。普通 reply 沿用 env/deny/验收/证据，fresh 更新 deny/standing，返工与容量读当前配置。

用户级示例：

```json
{"maxActive":32,"repoMaxActive":0,"maxRework":1,
 "models":{"sol":"provider/code-model","ds":"provider/fast-model"},
 "thinking":"high",
 "tiers":{"scout":"ds","review":"sol","write":[{"model":"sol","maxActive":10},"ds"]},
 "env":{"CUDA_VISIBLE_DEVICES":""},
 "agentDeny":[{"argv":["cargo","xtask","check"],"hint":"主控合入后跑全量；同事改跑 cargo test -p <crate>"}]}
```

## 同事全量检查拦截（agentDeny）

仓库根 `.delegate.json` 示例：

```json
{"agentDeny": [
  {"argv": ["cargo", "xtask", "check"], "hint": "全量检查由主控合入后跑；改跑 cargo test -p <crate>"},
  {"argv": ["cargo", "xtask", "infra-test"], "exact": true, "hint": "带过滤参数只测相关用例"}
]}
```

默认按完整 argv 大小写敏感前缀匹配；`exact: true` 要求 argv 完全相等（不忽略或重排 `+toolchain`/`--locked`）。规则 `argv` 非空、首项为程序名、`hint` 非空、无 NUL，同程序按序匹配首条。

启动时在 `agent-shims/` 生成 shell shim 并插入同事 PATH 前端（reply 继承）；命中输出 hint 到 stderr 并退出 77，未命中透传 exec。同事的 `lane` 同样继承 shim PATH。`denied.log` 累计次数，结论记 `denied: N`；进程匹配三个 shim 始终生成。验收、证据、setup、生成核对、apply 与主控 `lane` 不注入 shim。威胁模型是防误用，非安全沙箱。

- `stall`：`{"idle":"30m","command":"90m","accept":"10m"}`，`"0"` 关闭；`DELEGATE_STALL_IDLE`/`COMMAND`/`ACCEPT` 优先，均只发 `stall_warning` 不杀进程。
- `checkIn`：wait 检查线时长，`"0"` 关闭；`DELEGATE_CHECK_IN` 优先。未设置时 Claude Code 使用 50m，其它调用方不设默认。
- `agy`：`{"agent":"aia"}` 为 agy 同事附加 `--agent <名>`（新会话、续做与 `probe` 生效），名称非空且无空白/控制字符，未设不变。
- `review`（默认关）：`true` 或 `{"enabled"?,"tier"?:"review"|"scout","model"?,"thinking"?,"rubric"?}`（仓库优先）。写入验收通过后派新上下文只读审查员（要点默认 [review-rubric.md](review-rubric.md)）；首行严格 `PASS`/`REWORK`/`NEEDS_CONTROLLER`（畸形/越权写入失败关闭），`REWORK` 原样退回且续审只看增量 diff（第 3 轮起发 `needs_attention` 不强杀）。
- `controllerReviewPaths`：glob 数组（`*`/`?`/`**`/末尾 `/`），仓库优先。命中一律升 `NEEDS_CONTROLLER` 并附 diff 与 SHA-256，主控 `delegate sign <run>` 签字（改一字即失效，`sign --check` 验签）。
- `mergeAccept` / `autoMerge`（仓库级）：`mergeAccept` 为 `delegate merge` 终验命令（默认等于 `accept`）；`autoMerge` 为 glob 数组（默认空，支持 `!` 排除与 `add:` 纯新增），全改动命中且审查 `PASS` 标「可自主合入」，详见 [合入员与 Radar](merge-and-radar.md)。
- `radar`（仓库级）：`{"collect":[命令或{"name","command"}],"tier"?:"scout"|"review","model"?,"thinking"?,"timeout"?}`；确定性采集算指纹，未变零 token 退出，变化派只读 scout 更新 `<state>/radar/<repo>/board.md`，见 [合入员与 Radar](merge-and-radar.md)。
