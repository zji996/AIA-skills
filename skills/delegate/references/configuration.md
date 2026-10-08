# 用户与仓库配置、检查拦截

`XDG_CONFIG_HOME` 未设时使用系统约定的用户配置目录。

## 用户与仓库配置

用户配置 `${XDG_CONFIG_HOME}/delegate/config.json` 支持 `agentDeny`、`env`、`maxRework`、默认 `accept`/`evidence`、`maxActive`、`maxCodex`、`maxHeavy`、`repoMaxActive`、`repoMaxCodex`、`sourceBuild.paths`、`standing`、`defaults`、`resultChars`、`cleanupKeepExecutables`。仓库根 `.delegate.json` 使用相同字段并保存仓库事实。

- 标量：仓库 > 用户 > 内置默认；容量的 `DELEGATE_MAX_ACTIVE` / `DELEGATE_MAX_CODEX` / `DELEGATE_MAX_HEAVY` / `DELEGATE_REPO_MAX_ACTIVE` / `DELEGATE_REPO_MAX_CODEX` 最高，仍兼容 `PI_DELEGATE_*`，`0` 不限。整机默认 12 / 6 / 2，每仓库默认 8 / 4。写入默认 `accept`/`evidence` 在仓库未提供字段时用用户值，对应 CLI 覆盖或关闭；只读不用两项默认。
- 任务测试用可重复的 `--accept-also COMMAND`，按顺序以 `&&` 追加到生效的 `--accept` 或配置默认（无基础命令则单独运行）；与 `--no-accept` 冲突退出 2。`--accept` 替换非空且不同的默认命令时，start 提示原命令与追加用法；meta 的 `accept` 与 summary 的 `accept.command` 记录最终命令。
- `env` 按键合并，仓库优先；`agentDeny` 按 `(argv, exact)` 去重，省略 `exact` 等于 `false`；仓库同键 hint 覆盖用户值，保留原顺序。仓库 `{"argv":["cargo","xtask","check"],"exact":true,"allow":true}` 撤销完全相同 argv 且 exact 相同的一条用户规则，无需 hint；不撤销不同 exact 或其他前缀规则，用户配置不能声明 `allow: true`。
- `sourceBuild` 整个对象按仓库 > 用户 > 内置默认覆盖；`{"sourceBuild":{"paths":["target","build"]}}` 指定相对源仓库根的构建目录，替换默认的 Cargo.toml → target 探测，`paths: []` 关闭。两份配置的绝对路径、越界路径及外部符号链接独立报错（文件与 `sourceBuild.paths[index]`，退出 2），不存在的目录可配置。启动保存设置，reply 用当前配置；仅写入收尾测一次，提示阈值只用 `DELEGATE_SOURCE_BUILD_WARN_GIB`（默认 60 GiB，0 关闭），不从 config.env 读取提示阈值，与 worktree 阈值来源一致。缓存与输出见 [清理](cleanup.md)。
- `standing` 是固定说明：字符串、字符串数组，或 `{"all":"通用规矩","write":["写入规矩"],"readOnly":"只读规矩"}`；前两种等价于 `all`。按字段仓库覆盖用户，任务末尾附 `all` 与模式字段，再列生效的 deny 命令、匹配方式与 hint。启动回显附加行数及配置层，JSON 的 `standing`/`standingSources` 保存实际文字与来源；普通 reply 沿用会话，回显零行，`--fresh` 读取当前配置并附加。
- `defaults` 按字段仓库覆盖用户，支持 `worktree`（布尔）、`protect`/`acceptAlso`（字符串数组）、`timeout`（时长字符串）、`evidence`（命令字符串）；start/run 的 CLI 显式值覆盖整项数组（reply 的 accept-also 追加到继承验收），`--no-accept`/`--no-evidence` 关闭相应默认。除 timeout 外只用于写入；`--in-place` 显式原地写入，覆盖 `defaults.worktree: true`。未配置时行为不变；启动回显生效值和来源，JSON `defaults` 记录；reply 沿用已有设置。
- `resultChars` 是答复显示上限（正整数，默认 20000），环境变量 `DELEGATE_RESULT_CHARS` 优先；答复不超过上限的 1.25 倍时全文显示，超出才显示头尾。每任务 `--max-answer N` 是另一项答复约定，超过 N 的 1.5 倍时自动续接一次压缩，不计返工预算。
- `cleanupKeepExecutables` 是共享守护进程的可执行文件名数组，用户与仓库追加去重，内置 `sccache`；按实际可执行文件名识别，不能配置路径。回收不终止这些进程，普通进程提示名字和端口。
- `worktree`、`generated`、`applyVerify` 只认仓库：用户级出现时忽略，`start`（含 `run`）在 stderr 汇总提示一次。
- 两份文件独立校验，覆盖不能隐藏错误；错误指出文件与字段，启动退出 2。JSON 语法错误指出文件与行列。meta、summary 与 `--json` 记 `configSources: ["user","repo"]`（启动时只有存在的配置文件，没有时 `[]`）。普通 reply 沿用启动时的 env、deny、验收与证据，fresh 更新 deny 与固定说明，返工预算和容量按当前两级配置读取；来源包含继承配置与当前配置两部分。

用户级示例：

```json
{"maxActive":12,"maxCodex":6,"maxHeavy":2,"repoMaxActive":8,"repoMaxCodex":4,"maxRework":1,
 "env":{"CUDA_VISIBLE_DEVICES":""},
 "agentDeny":[{"argv":["cargo","xtask","check"],"hint":"主控合入后跑全量；同事改跑 cargo test -p <crate>"}]}
```

## 同事全量检查拦截（agentDeny）

仓库根 `.delegate.json` 可配置：

```json
{"agentDeny": [
  {"argv": ["cargo", "xtask", "check"], "hint": "全量检查由主控合入后跑；改跑 cargo test -p <crate> 与 cargo clippy -p <crate>"},
  {"argv": ["cargo", "xtask", "infra-test"], "exact": true, "hint": "请带过滤参数只测相关用例"},
  {"argv": ["pnpm", "check"], "hint": "全量检查由主控合入后跑；改跑 pnpm --filter <package> test"}
]}
```

默认按完整 argv 的逐项、大小写敏感前缀匹配；尾部可以有更多参数。`exact: true` 必须 argv 完全相等且无多余参数：上述 `cargo xtask infra-test` 命中，`cargo xtask infra-test --filter db` 放行。`cargo xtask check --all` 命中前缀规则，`cargo --locked xtask check` 不命中，不忽略或重排 `+toolchain`、`--locked` 等全局参数。每条规则的 `argv` 非空，首项是程序名（字母、数字、`._-`，不能是路径或 `.`/`..`），其他项为字符串，`hint` 非空，`exact` 可选且为布尔值；字符串不能含 NUL。同程序多条规则按配置顺序匹配首条。

启动时在 run 的 `agent-shims/` 为每个程序生成 shell shim，按原 PATH（含顶层 env.PATH）与同事工作目录解析真实程序的绝对路径，保留 cargo/rustup 等多调用符号链接的名称；真实程序缺失时，未命中的调用退出 127。只在同事进程的 PATH 最前面插入该目录，reply 继承规则。命中打印 hint 到 stderr，退出 77；未命中直接 exec，参数、stdin/stdout 与退出码透传。同事的 `lane` 继承 shim PATH，即使自己设置 `DELEGATE_ALLOW_HEAVY=1` 或 `DELEGATE_LANE_HELD=1` 仍会拒绝。`denied.log` 累计本轮各尝试的次数，结论 `denied: N`，短行显示“拦下 N 次全量检查”；未配置或空列表不生成 shim，不增加字段。

验收、证据、setup、收尾生成核对、apply 的生成与 applyVerify 都由未注入 shim 的执行环境运行，主控自己的 lane 同理。没有环境放行开关或可交给同事的令牌。威胁模型是防误用，不是防恶意：同 UID 进程可以改 PATH、用绝对路径或改 shim/记录，这不是安全沙箱。
