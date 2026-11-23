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

用户配置 `${XDG_CONFIG_HOME}/delegate/config.json` 支持 `remote`、`agentDeny`、`measure`、`measureCpus`、`env`、`maxRework`、默认 `accept`/`acceptTimeout`/`evidence`、`maxActive`、`maxHeavy`、`repoMaxActive`、`models`/`tiers`/`thinking`、`sourceBuild.paths`、`standing`、`defaults`、`resultChars`、`cleanupKeepExecutables`、`agy`。仓库根 `.delegate.json` 使用相同字段并保存仓库事实。

`measure` 是 argv 前缀数组，例如 `{"measure":[["cargo","xtask","bench-concurrency"]]}`；仓库配置覆盖用户配置。`measureCpus` 是预约核列表（如 `"8-11"`），未设置时取最后 `max(2, nproc/3)` 个在线逻辑核。匹配的同事、验收、证据和 lane 命令自动排队到机器级测量预约；预约开始时在跑的 lane 命令会让出预约核，预约期间普通 lane 命令使用其余核。

- 标量：仓库 > 用户 > 内置默认；容量的 `DELEGATE_MAX_ACTIVE` / `DELEGATE_MAX_HEAVY` / `DELEGATE_REPO_MAX_ACTIVE` 最高，`0` 不限。整机/仓库上限默认 32/0、重命令 2；实际限流靠模型 `maxActive`、`maxHeavy` 与内存水位，不足自动排队。写入默认 `accept`/`acceptTimeout`/`evidence` 仓库优先于用户、CLI 覆盖或关闭（`acceptTimeout` 可选、未设不限时，回显来源，`reply` 继承）；只读不继承。
- 模型选择独立为 `tiers.scout/review/write` 用途、`models` 登记别名与 `thinking` 长度；models/tiers 按键合并。模型全集只来自登记表，Pi 仅校验与提供能力；旧档与模型 thinking 后缀不兼容。匹配、规则、优先级、图片守卫、agy 同事与迁移见 [用途、模型与思维长度](model-routing.md)。
- 任务测试用可重复的 `--accept-also COMMAND`，按顺序以 `&&` 追加到生效的 `--accept` 或配置默认（无基础命令则单独运行）；与 `--no-accept` 冲突退出 2。`--accept` 替换非空且不同的默认命令时，start 提示原命令与追加用法；meta 的 `accept` 与 summary 的 `accept.command` 记录最终命令。
- `env` 按键合并，仓库优先；`agentDeny` 按 `(argv, exact)` 去重，省略 `exact` 等于 `false`；仓库同键 hint 覆盖用户值，保留原顺序。仓库 `{"argv":["cargo","xtask","check"],"exact":true,"allow":true}` 撤销完全相同 argv 且 exact 相同的一条用户规则，无需 hint；不撤销不同 exact 或其他前缀规则，用户配置不能声明 `allow: true`。
- `sourceBuild` 整个对象按仓库 > 用户 > 内置默认覆盖；`{"sourceBuild":{"paths":["target","build"]}}` 指定相对源仓库根的构建目录，替换默认的 Cargo.toml → target 探测，`paths: []` 关闭。两份配置的绝对路径、越界路径及外部符号链接独立报错（文件与 `sourceBuild.paths[index]`，退出 2），不存在的目录可配置。启动保存设置，reply 用当前配置；仅写入收尾测一次，提示阈值只用 `DELEGATE_SOURCE_BUILD_WARN_GIB`（默认 60 GiB，0 关闭）。缓存与输出见 [清理](cleanup.md)。
- `standing` 是固定说明：字符串、字符串数组，或 `{"all":"通用规矩","write":["写入规矩"],"readOnly":"只读规矩"}`；前两种等价于 `all`。按字段仓库覆盖用户，任务末尾附 `all` 与模式字段，再列生效的 deny 命令、匹配方式与 hint。启动回显附加行数及配置层，JSON 的 `standing`/`standingSources` 保存实际文字与来源；普通 reply 沿用会话，回显零行，`--fresh` 读取当前配置并附加。
- `defaults` 按字段仓库覆盖用户，支持 `worktree`、`keepCommits`（布尔）、`protect`/`acceptAlso`（字符串数组）、`timeout`（时长字符串）、`evidence`（命令字符串）；`keepCommits` 默认 false，CLI `apply --keep-commits` 可按次开启。启动选项的 CLI 显式值覆盖默认；`start`/`reply` 的 `--allow <path>` 可重复，只能在生效保护范围内按次开放文件/子目录（目录以 `/` 结尾，现存目录可省略），启动回显，任务说明列剩余保护与本次例外，收尾不将开放路径判违规；不改配置，后续 reply/fresh 继承。正文提及未开放的受保护路径时仅提示补 `--allow` 并在 `status`/`wait` 标记，不拦截。`worktree.cleanOnApply`（默认 true）控制 `apply` 成功后是否归档并删除 worktree。
- `resources` 按资源名仓库覆盖用户，例如 `{"pg":{"commands":[["cargo","xtask","infra-test"]]}}`；argv 逐项前缀匹配同 `agentDeny`。匹配命令开始执行时登记，结束删除；`busy` 也按 `/proc` 进程身份剔除已消失记录。只查询占用，不排队、不加锁；同事、验收和 lane 均登记。
- `acceptBlind` 是 glob 数组，例如 `["**/Dockerfile*","**/compose*.y*ml","manage.sh"]`，仓库值覆盖用户值。写入交付时命中路径仍可交付，但短行、JSON `acceptBlind` 与 apply 末行提示最多五个文件及其余数量。
- `resultChars` 是答复显示上限（正整数，默认 20000），环境变量 `DELEGATE_RESULT_CHARS` 优先；答复不超过上限的 1.25 倍时全文显示，超出才显示头尾。每任务 `--max-answer N` 是另一项答复约定，超过 N 的 1.5 倍时自动续接一次压缩，不计返工预算。
- `cleanupKeepExecutables` 是共享守护进程的可执行文件名数组，用户与仓库追加去重，内置 `sccache`；按实际可执行文件名识别，不能配置路径。回收不终止这些进程，普通进程提示名字和端口。
- `worktree`、`generated`、`applyVerify` 只认仓库：用户级出现时忽略，`start`（含 `run`）在 stderr 汇总提示一次。
- 两份文件独立校验，错误指出文件与字段并退出 2。meta、summary 与 `--json` 记 `configSources`（实际存在的 `user`/`repo`）。普通 reply 沿用启动时的 env、deny、验收与证据，fresh 更新 deny 与固定说明，返工预算和容量读当前配置。

用户级示例：

```json
{"maxActive":32,"maxHeavy":2,"repoMaxActive":0,"maxRework":1,
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
- `review`（默认关）：`true` 或 `{"enabled"?:true,"tier"?:"review"|"scout","model"?:"<别名>","thinking"?:"<档>","rubric"?:"<路径>"}`（仓库优先）。写入验收通过后派新上下文只读审查员（禁写文件/派活），输入任务说明、`diff --total` 与要点（默认 [review-rubric.md](review-rubric.md)）；首行严格 `PASS`/`REWORK`/`NEEDS_CONTROLLER`（格式不对或越权写入失败关闭为 `NEEDS_CONTROLLER`），`REWORK` 原样退回且续审只看增量 diff（不设轮数上限，第 3 轮起发 `needs_attention` 不强杀）。
- `controllerReviewPaths`：glob 数组（支持 `*`/`?`/`**`/末尾 `/`），仓库优先。净改动命中时一律升 `NEEDS_CONTROLLER` 并附 diff 全文与 SHA-256 哈希，主控用 `delegate sign <run>` 落盘 `sign.json`（改一字即失效，`sign --check` 校验）。
