## L0

### 1. 落点
1. [manage.sh](manage.sh#L854-L896) :: `start_server_delegated` (L854-896) — `systemd-run` 启动命令在此组装（含 `--collect`），需改为经「检查二进制落后→按需构建→失败落诊断」入口启动。
2. [manage.sh](manage.sh#L1029-L1061) :: `dev_restart_server` (L1029-1061) — 当前要求单元 `is-active` 才重启（L1031），失败停机后会被拒；需兼容失败态恢复与失败诊断。
3. [manage.sh](manage.sh#L1813-L1843) :: `dev_status` (L1813-1843) — 进程不存活时固定打印 `Server: stopped`（L1830），需改为优先读取 `~/.local/state/norvane/dev/` 启动失败诊断并在首行显示原因与修复命令。
4. [scripts/run-delegated.sh](scripts/run-delegated.sh#L5-L50) :: `--inside` (L5-50) — 被 `start_server_delegated` 直接作为单元入口并 `exec "$@"`；与 CI 委托单元共用，改动须隔离 dev 逻辑。
5. [apps/aiaxiom-server/src/server/threads/harness_change.rs](apps/aiaxiom-server/src/server/threads/harness_change.rs#L96-L133) :: `start_watcher` (L96-133) — 2 秒轮询 `agent.toml`，`reload_harness` 失败不更新 `last_pointer` 导致每 2 秒 WARN 刷屏；需对解析失败降为单次 WARN + 诊断，并可选 dev 触发重启。
6. [apps/aiaxiom-server/src/server/error_log.rs](apps/aiaxiom-server/src/server/error_log.rs#L141-L170) :: `startup_failure` / `background_diagnostic` (L141-170) — 启动失败只打 `server_error` JSON 到日志/stderr；`background_diagnostic` 可供热加载结构化诊断复用。
7. [apps/web/src/lib/devLogin.ts](apps/web/src/lib/devLogin.ts#L5-L17) :: `useDevAccounts` (L5-17) — 当前吞掉除 404 外的所有失败（L12 仅 `console.warn`），需向调用方区分 404 与 5xx/网络失败。
8. [apps/web/src/pages/Auth.tsx](apps/web/src/pages/Auth.tsx#L39-L279) :: `Auth` (L66, L190-194, L253-274) — 消费 `useDevAccounts`，在 5xx/网络失败时用现有 `.auth-error`（`role="alert"`）展示「服务暂时连不上」。
9. [apps/web/src/pages/home/AccountMenu.tsx](apps/web/src/pages/home/AccountMenu.tsx#L206-L209) :: `DevAccountItems` (L206-209) — `useDevAccounts` 的另一唯一调用方；改动 hook 返回类型时须同步适配（及 `AccountMenu.test.tsx`）。
10. [apps/web/src/i18n/zh/Auth.ts](apps/web/src/i18n/zh/Auth.ts#L1-L17) & [apps/web/src/i18n/en/Auth.ts](apps/web/src/i18n/en/Auth.ts#L1-L19) :: `Auth` (L1-19) — 登录页中英字典（`en` 受 `CatalogShape<typeof zh>` 校验），新增服务连不上提示文案。
11. [apps/web/src/pages/Auth.test.tsx](apps/web/src/pages/Auth.test.tsx#L77-L90) & [apps/web/tests/auth.spec.ts](apps/web/tests/auth.spec.ts#L9-L47) — 补充 5xx/网络失败与 404 对照用例。
12. [docs/reference/dev-maintenance.md](docs/reference/dev-maintenance.md#L193-L239) :: `## dev 新鲜度` / `## dev 日志` (L193-239) — 补充启动前对齐二进制与 `dev status` 展示启动失败两行说明。

### 2. 跨文件关系与验证命令
- **启动链**：`manage.sh dev up` → `start_server_background` → `start_server_delegated` → `systemd-run` → `scripts/run-delegated.sh --inside` → `aiaxiom-server serve`；`dev_restart_server` / `scripts/dev_fresh.py` / `scripts/host-upgrade-watch.py` 均经 `./manage.sh dev restart-server` 走 `systemctl --user restart`。
- **热加载链**：`server.rs:488` 启 `harness_change::start_watcher` → `reload_harness` → `HarnessStore::reload_from_dir` → `HarnessRevision::load_default` → `toml::from_slice`（错误归 `KernelError::Storage` → `ApiError`）；dev 模式标记在 `state.control.dev_login_enabled()`（源于 `AIAXIOM_DEV_LOGIN=1`）。
- **前端登录链**：`main.tsx:201-250` 遇 `/auth/session` 与 `/setup/status` 502 后回落渲染 `<Auth />` → `useDevAccounts()` 调 `api("/auth/dev-accounts")`（`packages/norvane-browser/src/core.ts`：网络失败抛 `ApiError(0, ..., "network")`，5xx 抛 `ApiError(status, ...)`）。
- **验证命令**：`bash -n manage.sh`（若装了则 `shellcheck manage.sh`）、新增的 `manage.sh` 检查函数单测（参考 `python3 -m unittest scripts/test_*.py`）、`cargo test -p aiaxiom-server --lib harness_change`、`pnpm --filter @norvane/web test`、`pnpm check:web`；E2E 跑 `pnpm --filter @norvane/web exec playwright test tests/auth.spec.ts`（需运行中的 dev 服务）。

### 3. 与说明不符之处
1. **本机未安装 `shellcheck`，且仓库门禁未接入 `shellcheck`**：`xtask/src/main.rs` 与 `scripts/gate.sh` 均无 `shellcheck` 步骤；脚本校验实际走 `bash -n` 与 `xtask/src/main.rs:400-418` 登记的 `python3 -m unittest`。
2. **`dev_restart_server` 无法修复已失败退出的单元**：`manage.sh:860` 的 `systemd-run` 带 `--collect`（单元失败即卸载）且 `manage.sh:1031` 要求 `systemctl --user is-active` 才重启。若诊断提示 `./manage.sh dev restart-server`，须同时让 `dev_restart_server` 支持非 active / 已卸载单元或去掉 `--collect`。
3. **仓库无 Stop 钩子或主控提醒入口**：无 `.claude/` / `.gemini/` Stop hook；`.githooks/post-{commit,merge}` 仅触发 `dev fresh --apply`。按说明只做 `dev status` 与诊断文件。
4. **`harness_change.rs` 当前无内联 `#[cfg(test)]`**：`cargo test -p aiaxiom-server --lib harness_change` 现有匹配数为 0，需新建单元测试模块。

### 4. 未核实项
- 本机实际运行的 `systemd --user` 版本下，去掉 `--collect` 后 `systemctl --user restart` 对 `Result=exit-code` transient unit 的精确行为，以及 `manage.sh` 包装脚本是否需要把 `PATH` 中 `~/.cargo/bin`（`load_rust_path`）注入 transient unit 环境（`manage.sh:866` 当前转发当前 shell 的 `PATH`，若外部环境未带 `~/.cargo/bin` 需在单元内兜底）— 实现时在 `manage.sh:854-896` 与 `dev_autostart_on`（L1251-1255）核对。
- `dev_fresh.py` 的 `fresh-apply.lock` 与热加载自动触发重启（若启用）之间的并发互斥细节 — 查 [scripts/dev_fresh.py](scripts/dev_fresh.py#L110) 与 `_restart_dev_server` (L2527)。

---

## L1

### 一、完整调用表与数据结构位置

#### 1. `manage.sh` 与 systemd 委托启动 / 重启 / 状态
- **全局路径与状态文件** — [manage.sh](manage.sh#L4-L62)
  - `NORVANE_STATE_DIR` (L5)、`DEV_DIR="${AIAXIOM_DEV_DIR:-$NORVANE_STATE_DIR/dev}"` (L16)
  - `PID_FILE` (L17)、`LOG_FILE` (L18)、`LISTEN_FILE` (L24)、`SERVER_UNIT_FILE` (L25)、`SERVER_PORT_FILE` (L27)、`SERVER_BIN="$ROOT_DIR/target/debug/aiaxiom-server"` (L62)
  - `load_rust_path()` (L142-147)：在 `cargo` 不在 `PATH` 时 source `$HOME/.cargo/env`
  - `read_server_pid()` (L149-155)、`server_running()` (L157-161)、`read_server_unit()` (L849-852)
- **委托启动** — [manage.sh](manage.sh#L854-L991)
  - `start_server_delegated()` (L854-896)：创建 transient unit `aiaxiom-local-dev-<ts>-<rand>`，设 `--collect`、`Delegate=cpu memory pids`、`TimeoutStopSec=10`、`StandardOutput/Error=append:$LOG_FILE`，转发 `PATH`、`COMPOSE_PROJECT_NAME`、`AIAXIOM_*`、`RUST_*`、`XDG_*` 与 `server_environment` 数组；L884 调用 `systemd-run "${arguments[@]}" /bin/bash "$ROOT_DIR/scripts/run-delegated.sh" --inside "$SERVER_BIN" serve`。
  - `start_server_background()` (L924-991)：组装 `server_environment`（含 `AIAXIOM_DEV_LOGIN=1` L936、`AIAXIOM_HARNESS_DIR` L941、`AIAXIOM_LISTEN_FILE` L935），按 `systemd` / `setsid` / `nohup` 分发。
  - `wait_for_server()` (L795-812)：轮询 `kill -0 "$pid"` 与 `GET /api/v1/health`，进程退出即返回 1（无时间上限，遵循 P9 / ADR 0120）。
- **停止与重启** — [manage.sh](manage.sh#L993-L1061)
  - `stop_server()` (L993-1014)、`stop_server_unit()` (L1017-1023)：发 SIGTERM/SIGKILL 并 `systemctl --user stop "$unit.service"`，删除 `SERVER_UNIT_FILE`、`PID_FILE`、`LISTEN_FILE`。
  - `dev_restart_server()` (L1029-1061)：L1031 校验 `read_server_unit` 且 `systemctl --user is-active --quiet "$unit.service"`；L1037-1038 `load_rust_path` + `cargo build --package aiaxiom-server`；L1046 `systemctl --user restart "$unit.service"`；L1048-1052 校验 `MainPID` 并 `wait_for_server`，失败仅打印 `tail -n 40 "$LOG_FILE"` 退出 1；L1054-1059 端口变化时同步重启 Vite 并 `configure_caddy`。
  - `dev_up()` (L1119-1230)：L1133 装 `.githooks`，L1181 `cargo build`，L1194 `start_server_background`，L1197-1203 启动失败时 `tail -n 80 "$LOG_FILE"` 并 `stop_server && compose down`。
- **状态与分发** — [manage.sh](manage.sh#L1813-L1974)
  - `dev_status()` (L1813-1843)：`server_running` 假时仅 `echo "Server: stopped"`（L1830）。
  - CLI 分发 `dev` 子命令 (L1930-1974)：`up / autostart / down / restart / restart-server / restart-web / fresh / seed / purge / status / ports / logs`。
- **委托 cgroup 包装脚本** — [scripts/run-delegated.sh](scripts/run-delegated.sh#L1-L90)
  - `--inside` 分支 (L5-50)：校验 `/proc/self/cgroup` 为 `aiaxiom-local-*.service` 或 `norvane-ci-delegated-*.service`（L9-11），创建 `supervisor/` 与 `jobs/` 子 cgroup，设 `AIAXIOM_CGROUP_ROOT="$delegate_root/jobs"`（L33），最后 `exec "$@"`（L49）。
  - 注意：该脚本也用于 CI 委托单元（`norvane-ci.slice`）和 `./manage.sh dev doctor`（`manage.sh:1936`）。

#### 2. Server 启动失败与 Harness 热加载
- **启动失败入口**
  - [apps/aiaxiom-server/src/main.rs](apps/aiaxiom-server/src/main.rs#L11-L29) :: `main` / `run` — 捕获 `aiaxiom_server::run().await` 返回的 `Err`，调 `aiaxiom_server::log_startup_failure(&error)` 后以 `ExitCode::FAILURE` 退出。
  - [apps/aiaxiom-server/src/server.rs](apps/aiaxiom-server/src/server.rs#L267-L269) :: `pub fn log_startup_failure(error: &anyhow::Error)` — 转发给 `error_log::startup_failure(error)`。
  - [apps/aiaxiom-server/src/server/error_log.rs](apps/aiaxiom-server/src/server/error_log.rs#L124-L170) :: `startup_record` / `startup_failure` / `background_diagnostic` — 格式化 `schema: 1, kind: "startup_failure", reason: format!("{error:#}")` 到 tracing error 或 stderr `ERROR server_error=...`。
- **AppState 与 dev 开关**
  - [apps/aiaxiom-server/src/server.rs](apps/aiaxiom-server/src/server.rs#L216-L250) :: `struct AppState` — 持有 `control: Arc<ControlPlane>`、`harness: HarnessStore`、`shutdown: shutdown::Shutdown` 等。
  - [apps/aiaxiom-server/src/server.rs](apps/aiaxiom-server/src/server.rs#L344) — `.with_dev_login(std::env::var("AIAXIOM_DEV_LOGIN").as_deref() == Ok("1"))`；运行时通过 `state.control.dev_login_enabled()`（[crates/control-plane/src/auth.rs](crates/control-plane/src/auth.rs#L112-L114)）读取。
  - [apps/aiaxiom-server/src/server.rs](apps/aiaxiom-server/src/server.rs#L486-L488) — 服务启动时依次调用 `scan_resident_harness_changes`、`schedule_archive_prune`、`start_watcher(state.clone())`。
- **Harness 热加载与错误链路**
  - [apps/aiaxiom-server/src/server/threads/harness_change.rs](apps/aiaxiom-server/src/server/threads/harness_change.rs#L79-L133) :: `reload_harness(state: &AppState) -> Result<bool, ApiError>` (L79-93) & `start_watcher(state: AppState)` (L96-133) — `WATCH_INTERVAL = 2s`（L11）；对比 `agent.toml` 字节变化后调 `reload_harness`；仅在 `Ok(_)` 时更新 `last_pointer = current_pointer`，`Err(error)` 时打 `tracing::warn!(?error, "harness reload deferred; will retry")`（L128）且不更新状态，导致每 2 秒重复解析与告警。
  - [crates/runtime/src/harness/archive.rs](crates/runtime/src/harness/archive.rs#L377-L389) :: `pub fn reload_from_dir(&self) -> Result<Option<Arc<HarnessRevision>>, KernelError>` — 调用 `HarnessRevision::load_default(dir)?`。
  - [crates/runtime/src/harness/mod.rs](crates/runtime/src/harness/mod.rs#L326-L380) :: `HarnessRevision::load_default` / `load_pointer` — `agent.toml` 解析错误返回 `KernelError::Storage("invalid Harness default pointer: ...")`（L374）；`recipes.toml` 解析错误在 L651-652 返回 `"invalid recipes.toml: {error}"`（含 `unknown field ...`），经上层统一包装为 `KernelError::Storage(...)`，再经 [apps/aiaxiom-server/src/server/api_error.rs](apps/aiaxiom-server/src/server/api_error.rs#L509) 转为 `ApiError`。

#### 3. 前端登录页与 `devLogin`
- **API 客户端错误语义** — [packages/norvane-browser/src/core.ts](packages/norvane-browser/src/core.ts#L207-L306)
  - `export class ApiError extends Error` (L207-217)：字段 `status: number`, `message: string`, `code?: string`, `details?: unknown`。
  - `requestApi` (L267-306)：`fetch` 抛异常时（网络断开/连接被拒）抛 `new ApiError(0, ..., "network")`（L289）；HTTP 502+ 且无 JSON error message 时抛 `new ApiError(response.status, t("shared.serviceTemporarilyUnavailableTryAgainLater"), ...)`（L297-303，文案在 [packages/norvane-browser/src/i18n/zh/shared.ts](packages/norvane-browser/src/i18n/zh/shared.ts#L65) 为「服务暂时不可用，请稍后重试」）。
- **进入登录页的路径** — [apps/web/src/main.tsx](apps/web/src/main.tsx#L195-L250) & [L355-371](apps/web/src/main.tsx#L355-L371)
  - `Screens` 首屏调 `/auth/session` 失败（含 502）后清空 `actor`，再调 `getSetupStatus()` 失败也吞掉，最终落入 `!actor` 分支渲染 `<Auth />`。
- **dev 账号拉取与消费**
  - [apps/web/src/lib/devLogin.ts](apps/web/src/lib/devLogin.ts#L5-L17) :: `export function useDevAccounts()` — 请求 `api<DevAccountsView>("/auth/dev-accounts")`，404 静默、其余仅 `console.warn`，返回值恒为 `accounts | undefined`。
  - [apps/web/src/pages/Auth.tsx](apps/web/src/pages/Auth.tsx#L66) & [L190-194](apps/web/src/pages/Auth.tsx#L190-L194) & [L253-274](apps/web/src/pages/Auth.tsx#L253-L274) — 登录页已有 `.auth-error`（`role="alert"`，样式在 [apps/web/src/styles/auth.css](apps/web/src/styles/auth.css#L239-L248)）用于展示登录错误；`accounts` 区块在 L253-274。
  - [apps/web/src/pages/home/AccountMenu.tsx](apps/web/src/pages/home/AccountMenu.tsx#L206-L209) :: `DevAccountItems` — 另一唯一调用 `useDevAccounts()` 处，只在账号菜单展开时渲染切账号子菜单。
  - [apps/web/src/i18n/zh/Auth.ts](apps/web/src/i18n/zh/Auth.ts#L1-L17) & [apps/web/src/i18n/en/Auth.ts](apps/web/src/i18n/en/Auth.ts#L1-L19) — 无 snapshot 依赖，只需中英双文件成对增键。

### 二、可参照的同类实现
- **构建指纹与新鲜度检查**：[apps/aiaxiom-server/build.rs](apps/aiaxiom-server/build.rs#L1-L57)（编译期注入 `AIAXIOM_BUILD_COMMIT`）与 [scripts/dev_fresh.py](scripts/dev_fresh.py#L652) :: `check_server_freshness`（比较 `crates/`、`apps/aiaxiom-server/`、`harnesses/`、`skills/`、`packages/capabilities/` 变更）及 `_restart_dev_server` (L2527) 与 `APPLY_LOCK_FILENAME = "fresh-apply.lock"` (L110)。
- **dev 服务状态诊断**：[scripts/boot_doctor.py](scripts/boot_doctor.py#L191-L253) :: `check_dev_server`（读 `AIAXIOM_DEV_DIR` 下的 `aiaxiom-server.pid` / `aiaxiom-server.listen` 并探测 `/api/v1/health`）。
- **单次告警去重**：[docs/reference/dev-maintenance.md](docs/reference/dev-maintenance.md#L10)（jemalloc 采样失败只告警一次）；结构化后台诊断参照 [apps/aiaxiom-server/src/server/error_log.rs](apps/aiaxiom-server/src/server/error_log.rs#L156-L170) :: `background_diagnostic`。
- **外部脚本监听触发 `restart-server`**：[scripts/host-upgrade-watch.py](scripts/host-upgrade-watch.py) 与 `.githooks/post-commit`。

### 三、现成测试夹具写法
- **Shell / Python 运维脚本单测**：[scripts/test_dev_fresh.py](scripts/test_dev_fresh.py#L624-L680)（用 `tempfile.TemporaryDirectory` 构造假仓库与 stub `manage.sh` / fake runner，跑 `python3 -m unittest`；若要纳入 `cargo xtask check` 的 `python` 步骤，需在 [xtask/src/main.rs](xtask/src/main.rs#L400-L418) 的 unittest 列表登记）。
- **Harness 热加载测试**：
  - 纯单元测试可在 [apps/aiaxiom-server/src/server/threads/harness_change.rs](apps/aiaxiom-server/src/server/threads/harness_change.rs) 内联 `#[cfg(test)] mod tests` 抽取状态机/分类与去重逻辑，无需 PG（直接满足 `cargo test -p aiaxiom-server --lib harness_change`）。
  - 完整 `AppState` + `HarnessStore` 集成夹具见 [apps/aiaxiom-server/src/server/agents/tests/package_change.rs](apps/aiaxiom-server/src/server/agents/tests/package_change.rs#L1-L120)（依赖 PG，不适合作为轻量 lib 单测）。
- **登录页组件测试与 E2E**：
  - [apps/web/src/pages/Auth.test.tsx](apps/web/src/pages/Auth.test.tsx#L11-L14) & [L77-90](apps/web/src/pages/Auth.test.tsx#L77-L90)：已 mock `@norvane/browser` 的 `api` 为 `list`，并用 `list.mockRejectedValue(new ApiError(404, "Not found"))` 测试 404，可直接追加 `new ApiError(502, ...)` 与 `new ApiError(0, "Failed to fetch", "network")` 用例。
  - [apps/web/src/pages/home/AccountMenu.test.tsx](apps/web/src/pages/home/AccountMenu.test.tsx#L17-L21) & [L56](apps/web/src/pages/home/AccountMenu.test.tsx#L56)：mock `useDevAccounts` 返回 `undefined`，改返回值类型时需同步更新 mock。
  - [apps/web/tests/auth.spec.ts](apps/web/tests/auth.spec.ts#L14-L32)：通过 `page.route("**/api/v1/**", ...)` 拦截 `/auth/dev-accounts`。

### 四、看过但无关的文件
- [apps/aiaxiom-server/src/server/account.rs](apps/aiaxiom-server/src/server/account.rs#L187-L206) — 后端 `/auth/dev-accounts` 仅按 `dev_login_enabled` 注册路由，无须改动。
- [apps/web/tests/oauth-auth.spec.ts](apps/web/tests/oauth-auth.spec.ts#L104) / [apps/web/tests/multi-signup.spec.ts](apps/web/tests/multi-signup.spec.ts#L378) — 对 `/auth/dev-accounts` 均明确返回 404，只要 404 保持静默即不受影响。
- [crates/kernel/src/diary.rs](crates/kernel/src/diary.rs#L28-L39) — 负责 harness 变更后的日记封存标记，与热加载失败告警/重启无关。
- [scripts/alpha.sh](scripts/alpha.sh) — alpha 生产发布脚本，本次仅改 dev 单元。
