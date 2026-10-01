# Rust：编译与链接反馈

## 缓存边界

- 默认 dev 模式下，sccache 主要缓存可缓存的第三方依赖。
- workspace 成员/path 依赖默认增量编译，sccache 不缓存增量编译。
- 调用系统链接器的 crate 也不可缓存，不能指望缓存测试二进制。
- 同一源码在不同绝对目录可能不命中；workspace 增量状态不能跨路径搬用。
- 工具链、features、flags 不同也会影响命中；共享缓存不等于共享 target。
- 不要为了 sccache 一概禁用自有代码的增量编译，先比较热改动反馈。
- `sccache --show-stats` 看命中及不可缓存原因，别只确认进程存在。

## 用户级工具

`scripts/iteration-speed rust-setup --print` 预览配置。
Linux 用 mold 缩短链接；需编译器驱动能识别 `-fuse-ld=mold`。
用户级 `~/.cargo/config.toml` 示例：

```toml
[build]
rustc-wrapper = "sccache"

[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

`~/.config/sccache/config` 的 `[cache.disk] size` 以字节计。
用户容量限制不能写进仓库；sccache 配置默认路径依 OS 不同。
macOS 保留 Xcode 15+ 默认的新链接器（ld-prime），不要配置 Linux mold。
探测环境变量和仓库配置的覆盖；看到用户配置不代表它一定生效。

## 仓库级设置与迭代命令

```toml
[profile.dev]
debug = "line-tables-only"
[profile.test]
debug = "line-tables-only"
```

这保留回溯行号，减少调试信息；需要局部变量调试时再启用完整信息。
该字符串值要求 Rust 1.71+；应遵守项目 MSRV。
`cargo test -p <包>` 用于迭代，跨包契约改变时补下游测试。
固定 build/test 或 check 模式；不同模式、profile/features 可能重编依赖。
必要时用户级设置独立 `CARGO_TARGET_DIR`，再比较空间和时间成本。
依赖链最长路径标出传播范围；实际编译成本决定时间，长度不是耗时。
`tests/*.rs` 通常各自生成二进制；链接占主导时用单一入口加子模块组织测试。
文件夹模块避免被自动识别为额外入口；合并前检查隔离和共享状态。

dev/test profile 中依赖未优化时，测试 CPU 常被哈希、JSON Schema、regex 等依赖吃掉；先给依赖设置：
```toml
[profile.dev.package."*"]
opt-level = 2
```
再实测自有 crate 在 `[profile.dev]` 下调到 `opt-level = 1` 时的增量编译与测试耗时，再决定；test 默认继承 dev，显式覆盖时须核对。这些 opt-level 设置不改变 debug assertions 与溢出检查，release 不变。
带 feature 的门禁不要整套重跑同一批单元测试，只跑需要该 feature 的测试。

## Polaris 基线（任务提供的实测，非通用承诺）

12 核、8 万行 / 5 个 crate 直线依赖，约 470 个第三方依赖：
- 冷编译整个 workspace：无缓存、GNU ld 68 s；sccache+mold 63 s。依赖在 12 核上本来就快，冷编译大头是自有大 crate，缓存与快链接器只省约 5 s。
- 热改动：无改动 0.2 s；改下游 5.4 s；改最上游 6.9 s。
- build 后首次 check 35.9 s，交替模式多花约 30 秒。
- 调试信息：line-tables-only、依赖关调试信息、全关调试信息三者热改动都约 5 s，测试执行差 ≤4%，只省磁盘（5.1→4.0 GB），不值得牺牲行号。
- 真正的大头是测试执行：`cargo test -p aiaxiom-runtime --lib` 141 s，编译只占约 5 s；先找最慢的测试（真实等待、超时、子进程），再谈编译优化。
- 慢在排队而不是编译：并行 Agent 各跑全量门禁时，单项排队曾达 49 分钟；改为同事只跑相关检查、主控按批跑全量后基本消除。
此基线不代表当前结构或所有 Cargo 版本；复测后再决策。

## 核对来源

- [sccache Rust 限制](https://github.com/mozilla/sccache/blob/main/docs/Rust.md)
- [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
- [Cargo 配置](https://doc.rust-lang.org/cargo/reference/config.html)
- [sccache 配置](https://github.com/mozilla/sccache/blob/main/docs/Configuration.md)
