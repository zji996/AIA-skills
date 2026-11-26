## L0

### 1. 落点
- `apps/aiaxiom-server/src/server/ops.rs` :: `OpsAction` (L175-L202) — 新增 `AuthProvider` 子命令变体及模块声明 `mod auth_provider;`。
- `apps/aiaxiom-server/src/server/ops.rs` :: `log_writer` (L572-L591) — 将 `OpsAction::AuthProvider { .. }` 加入 `to_stderr`，避免 tracing 日志污染 stdout。
- `apps/aiaxiom-server/src/server/ops.rs` :: `run` (L594-L607) — 分发 `OpsAction::AuthProvider { action }` 到新模块。
- `apps/aiaxiom-server/src/server/ops/auth_provider.rs` :: 新建模块 — 实现 `list / set / disable` 的 clap 定义、凭据文件解析（去首尾空白、缺行报错、绝不把密钥进日志/错误/输出）、`ControlPlane` 加载、格式化输出及单测/DB 测试。
- `apps/aiaxiom-server/src/server/oauth/mod.rs` :: `is_provider_kind_supported` (L230-L232) — 若 `ops set` 启用时复用支持类型校验，将可见性从私有改为 `pub(super)`（`PublicUrl` 与 `callback_url` 已是 `pub(super)`）。
- `crates/control-plane/src/oauth/mod.rs` :: `ControlPlane` (L501-L876) — 已有 `list_auth_providers_on_host` / `upsert_auth_provider_on_host`；如需原子 `disable`（不经 read-modify-write），可在此新增 `disable_auth_provider_on_host` 并写 `audit_system_in`。
- `scripts/alpha.sh` :: `main` (L1392-L1430) 与 `usage` (L50-L76) — 如需支持 `./manage.sh alpha ops …`，须新增 `ops)` 分支转发 `server_cmd "$CURRENT/bin/aiaxiom-server" ops "$@"`（或明确改为走现有 `./manage.sh alpha server ops …`）。
- `docs/reference/dev-maintenance.md` :: 在线宿主机账号止血段 (L146-L149) — 紧邻 `ops account unbind` 补充 `ops auth-provider` 三行用法。

### 2. 跨文件关系与测试命令
- 入口链路：`main.rs::run` → `server.rs::run` (L271-L310) 解析 `Args`、初始化 `PgPool`（不迁移、不取 runtime lock）→ `ops::run` (L594) → `ops::auth_provider::run` → `decode_master_key`（读 `args.master_key_file`）+ `HarnessRevision::load_processors` + `ControlPlane::new` → 调用 `ControlPlane::list_auth_providers_on_host` / `upsert_auth_provider_on_host`（内部走 `upsert_auth_provider_inner` 加密入 `encrypted_secrets` 并 `audit_system_in` 写 `audit_events`）；回调地址来自 `oauth::PublicUrl::parse_optional(args.public_url.as_deref())` 与 `PublicUrl::callback_url`。
- 相关测试命令：
  - `cargo test -p aiaxiom-server --lib ops`
  - `bash scripts/run-delegated.sh cargo xtask infra-test -p aiaxiom-server auth_provider`
  - `cargo clippy -p aiaxiom-server --all-targets -- -D warnings`（若动了 `aiaxiom-control-plane` 则加测 `cargo test -p aiaxiom-control-plane` 与 `cargo clippy -p aiaxiom-control-plane --all-targets -- -D warnings`）。

### 3. 与说明不符之处
1. **通道是实例级而非组织级（`--org` 无落点）**：`aiaxiom_control.auth_providers`（`202611260539_oauth_identities.sql` L11-L27）无 `organization_id` 列，`encrypted_secrets.organization_id = NULL`，控制面 `list_auth_providers_on_host` / `upsert_auth_provider_on_host` 均无组织参数，审计记 `organization_id = NULL`（ADR 0187 §6：通道本身是实例级，仅 single 模式下允许组织管理员经 `/api/v1/o/{org}/admin/auth-providers` 管理）。
2. **`scripts/alpha.sh` 约 616 行的 `ops` 不是 `./manage.sh alpha ops`**：`ops()` (L616) 只是内部辅助函数，硬编码调用 `ops maintenance "$@"`；`main()` (L1392-L1430) 没有 `ops)` 分支，现有通用透传是 `./manage.sh alpha server <子命令>` (L1423-L1426)。
3. **不可照搬 `agent_move.rs` 的主密钥加载**：`agent_move.rs` (L240-L248) 在 `master_key_file` 缺失时静默回退 `[0_u8; 32]`；`auth-provider` 读写 `encrypted_secrets` 必须失败关闭（P7），应参照 `ops.rs` L278（`UpgradeGif`）直接读并 `decode_master_key`。
4. **控制面无独立 `disable` 方法**：仅有 `upsert_auth_provider_on_host` / `rotate_auth_provider_secret_on_host` / `delete_auth_provider_on_host`。
5. **`AuthProviderAdminView` 字段差异**：结构体存完整 `client_id` 和 `secret_last_four`（schema 约束 `secret_id NOT NULL`，故已建通道必有密钥）；`list` 要打印 `client_id` 尾 4 位需在 CLI 输出层截取。

### 4. 未核实项
- 未确认主控对「去掉 `--org`」以及「在 `scripts/alpha.sh` 新增 `ops)` 子命令 vs 沿用 `alpha server ops`」的取舍（需开工前由主控拍板，P6）。
- 未确认 `ops auth-provider set` 更新已有通道时，不传 `--credentials-file`（或凭据文件仅给一行）是否支持保留原密钥——`upsert_auth_provider_on_host` 已支持 `client_secret: None` 保留原值，但 CLI 参数层面需明确更新时如何传 `client_id` 与省略密钥。

---

## L1

### 一、核心调用链与数据结构位置

#### 1. CLI 入口与 `ops` 分发（`apps/aiaxiom-server`）
- `apps/aiaxiom-server/src/main.rs` :: `run` (L19-L29) — 调用 `aiaxiom_server::run().await`。
- `apps/aiaxiom-server/src/server.rs` :: `Args` (L140-L189) — 顶层 CLI/环境变量参数：
  - `database_url` (`AIAXIOM_DATABASE_URL`, L148)
  - `state_dir` (`AIAXIOM_STATE_DIR`, L152)
  - `master_key_file` (`AIAXIOM_MASTER_KEY_FILE`, L160)
  - `public_url: Option<String>` (`AIAXIOM_PUBLIC_URL`, L170)
  - `harness_dir` (`AIAXIOM_HARNESS_DIR`, L174)
  - `organizations_mode: OrganizationsMode` (`AIAXIOM_ORGANIZATIONS_MODE`, L186)
- `apps/aiaxiom-server/src/server.rs` :: `Command` (L191-L213) — `Command::Ops { action: ops::OpsAction }` (L209-L212)。
- `apps/aiaxiom-server/src/server.rs` :: `run` (L271-L310) — 先按 `ops::log_writer(&args)` 初始化 tracing，建库池 `create_control_pool_with_mode(&args.database_url, args.organizations_mode)` (L295)，`Some(Command::Ops { action }) => return ops::run(&args, &pool, action).await` (L308)，不跑迁移也不取 `hold_runtime_lock`。
- `apps/aiaxiom-server/src/server/ops.rs` :: `decode_master_key` (L58-L65) — `fn decode_master_key(bytes: &[u8]) -> Result<[u8; 32]>`（私有于 `ops` 模块，子模块可直接 `super::decode_master_key`）。
- `apps/aiaxiom-server/src/server/ops.rs` :: `OpsAction` (L175-L202) — 现有变体：`Agent`、`Debrief`、`Maintenance`、`Account`、`SelfPortrait`、`VerifyRequests`。
- `apps/aiaxiom-server/src/server/ops.rs` :: `log_writer` (L572-L591) — 控制 tracing 输出到 stderr 还是 stdout。
- `apps/aiaxiom-server/src/server/ops.rs` :: `run` (L594-L607) — `pub(super) async fn run(args: &Args, pool: &PgPool, action: &OpsAction) -> Result<()>`。

#### 2. 服务端 OAuth 路由与 `PublicUrl`（`apps/aiaxiom-server`）
- `apps/aiaxiom-server/src/server/oauth/mod.rs` :: `PublicUrl` (L52-L120)：
  - `pub(super) fn parse_optional(raw: Option<&str>) -> anyhow::Result<Option<Self>>` (L57-L62)
  - `pub(super) fn parse(raw: &str) -> anyhow::Result<Self>` (L64-L93)
  - `pub(super) fn callback_url(&self, provider_id: &str) -> String` (L104-L106)，格式 `{origin}/api/v1/auth/oauth/{provider_id}/callback`。
- `apps/aiaxiom-server/src/server/oauth/mod.rs` :: `admin_routes` (L221-L228) — 挂在 `/api/v1/o/{org_id}` 下：`GET /admin/auth-providers`、`PUT/DELETE /admin/auth-providers/{provider_id}`。
- `apps/aiaxiom-server/src/server/oauth/mod.rs` :: `is_provider_kind_supported` (L230-L232) — `fn is_provider_kind_supported(kind: AuthProviderKind) -> bool`（当前仅 `AuthProviderKind::Github` 为 `true`）。
- `apps/aiaxiom-server/src/server/oauth/account_admin.rs` :: `upsert_auth_provider` (L337-L377) — 管理接口参照逻辑：当 `input.enabled == true` 时检查 `is_provider_kind_supported(input.kind)` 与 `state.public_url.is_some()`，随后调用 `state.control.upsert_auth_provider(&actor, &provider_id, input)`。

#### 3. 控制面 OAuth 模型与方法（`crates/control-plane`）
- `crates/control-plane/src/lib.rs` :: `ControlPlane::new` (L298-L330)：
  - `pub fn new(pool: PgPool, master_key: [u8; 32], state_dir: impl Into<PathBuf>, harness: Arc<aiaxiom_runtime::HarnessRevision>) -> Result<Self, KernelError>`
- `crates/control-plane/src/models/oauth.rs`（全部经 `crates/control-plane/src/lib.rs` L118 `pub use models::*;` 导出）：
  - `AuthProviderKind` (L16-L45) — `Github | Wechat | Qq | Oidc`，带 `as_str` / `parse_db`。
  - `AuthProviderConfig` (L100-L117) — `proxy_url / issuer_url / scopes / qq_use_unionid`，实现 `Default`。
  - `AuthProviderAdminView` (L124-L136) — `id, kind, display_name, enabled, client_id, secret_last_four, config, sort_order, bound_count, created_at, updated_at`。
  - `UpsertAuthProviderInput` (L152-L170) — `kind, display_name, enabled, client_id, client_secret: Option<String>, config, sort_order`。
- `crates/control-plane/src/oauth/mod.rs` :: `ControlPlane`：
  - `pub async fn list_auth_providers(&self, actor: &ActorSession) -> Result<Vec<AuthProviderAdminView>, KernelError>` (L491-L497) — 鉴权后委托 `list_auth_providers_on_host`。
  - `pub async fn list_auth_providers_on_host(&self) -> Result<Vec<AuthProviderAdminView>, KernelError>` (L501-L518) — 查询全部通道（含禁用的），联表 `encrypted_secrets.last_four` 与 `bound_count`，按 `sort_order ASC, id ASC`。
  - `pub async fn resolve_auth_provider(&self, provider_id: &str) -> Result<AuthProviderRuntimeConfig, KernelError>` (L522-L559) — 解密密钥（`AuthProviderRuntimeConfig` 的 `Debug` 已脱敏为 `"[redacted]"`，L45-L57）。
  - `pub async fn upsert_auth_provider(&self, actor: &ActorSession, provider_id: &str, input: UpsertAuthProviderInput) -> Result<AuthProviderAdminView, KernelError>` (L563-L572) — 鉴权后走 `upsert_auth_provider_inner(Some(actor), ...)`。
  - `pub async fn upsert_auth_provider_on_host(&self, provider_id: &str, input: UpsertAuthProviderInput) -> Result<AuthProviderAdminView, KernelError>` (L576-L583) — 直接走 `upsert_auth_provider_inner(None, ...)`。
  - `upsert_auth_provider_inner` (L585-L786) — 核心写逻辑：
    - 校验 `provider_id` (`^[a-z0-9][a-z0-9_-]{0,63}$`)、`display_name` (<=60)、`client_id` (<=512)、`config`、`client_secret` (非空单行 <=4096)。
    - 首次创建要求 `client_secret` 存在；更新时 `None` 或 `Some("")` 保留旧 `secret_id`；给新密钥则插入新密文（`key_version + 1`）并同事务删除旧密文；更新禁止改 `kind`。
    - `actor.is_none()` 时调 `audit_system_in` 写 `event_type = "auth.provider_upserted"`（`subject_type = "auth_provider"`, `subject_id = provider_id`, `details = {kind, enabled, created, secretRotated, maintenance: true}`）；若轮换密钥另写 `"auth.provider_secret_rotated"`。
  - `pub async fn rotate_auth_provider_secret_on_host(&self, provider_id: &str, client_secret: &str) -> Result<AuthProviderAdminView, KernelError>` (L790-L857)。
  - `pub async fn delete_auth_provider_on_host(&self, provider_id: &str) -> Result<(), KernelError>` (L874-L876)。
- `crates/control-plane/src/audit.rs` :: `audit_system_in` (L34-L53) & `audit_account_in` (L56-L80) — 写入 `aiaxiom_control.audit_events`，`organization_id / workspace_id / actor_id / account_id` 均为 `NULL`。
- `crates/control-plane/migrations/202611260539_oauth_identities.sql` :: `auth_providers` (L11-L27) — 主键 `id text`，`secret_id uuid NOT NULL REFERENCES encrypted_secrets(id)`，无组织列。

#### 4. Alpha 脚本与文档
- `scripts/alpha.sh` :: `write_server_env` (L137-L158) — 写入 `AIAXIOM_DATABASE_URL`、`AIAXIOM_STATE_DIR`、`AIAXIOM_MASTER_KEY_FILE`、`AIAXIOM_PUBLIC_URL`、`AIAXIOM_ORGANIZATIONS_MODE=single`、`AIAXIOM_HARNESS_DIR`。
- `scripts/alpha.sh` :: `server_cmd` (L592-L604) — source `$SERVER_ENV` 后执行二进制，支持前置 `KEY=VAL` 覆盖（如 `RUST_LOG=warn`）。
- `scripts/alpha.sh` :: `ops_with` / `ops` (L608-L616) — 仅封装 `ops maintenance`。
- `scripts/alpha.sh` :: `main` (L1392-L1430) — `maintenance)` (L1422)、`server)` (L1423-L1426)、`account)` (L1427)。
- `manage.sh` :: `alpha)` (L2010) — 直接 `exec "$ROOT_DIR/scripts/alpha.sh" "$@"`。
- `docs/reference/dev-maintenance.md` (L146-L149) — 在线 `ops` 命令文档区（当前有 `ops account unbind`）。
- `docs/reference/release.md` (L9-L16) — 说明 `./manage.sh alpha server <子命令>` 的使用约定。

---

### 二、可参照的同类实现
1. **在线 `ops` 严格加载主密钥与 `ControlPlane`**：`apps/aiaxiom-server/src/server/ops.rs` :: `offline_maintenance` 中 `MaintenanceAction::UpgradeGif` 分支 (L270-L297) — 直接 `decode_master_key(&fs::read(&args.master_key_file).await?)?` + `HarnessRevision::load_processors(&args.harness_dir)?` + `ControlPlane::new(pool, key, &args.state_dir, harness)?.with_organizations_mode(args.organizations_mode)`，不取锁、不迁移、缺密钥失败关闭。
2. **`ops` 子命令组织与 clap 单测**：`apps/aiaxiom-server/src/server/ops/agent_move.rs` :: `AgentAction` (L45-L120) 与 `tests` (L1230-L1336)；`apps/aiaxiom-server/src/server/ops.rs` :: `AccountAction::Unbind` (L217-L234) 与 `ops_account_unbind_requires_provider_or_all_and_defaults_to_preview` (L902-L975)。
3. **Host 侧 OAuth 控制面调用**：`crates/control-plane/src/oauth/tests.rs` (L449-L467) — 直接使用 `upsert_auth_provider_on_host` 写入 GitHub/WeChat 通道。

---

### 三、现成测试夹具
- **纯单元测试（无需 DB）**：`apps/aiaxiom-server/src/server/ops.rs` :: `mod tests` (L665-L1002) 与 `agent_move.rs` (L1230-L1336)，使用 `<Args as clap::Parser>::try_parse_from([...])` 测 CLI 参数解析；凭据文件读取与脱敏格式化可直接写纯函数单测并用 `tempfile::NamedTempFile`（`tempfile` 已在 dev-dependencies 中，见 `test_support.rs` L318）。
- **`aiaxiom-server` PostgreSQL 集成测试**：
  - `apps/aiaxiom-server/src/server/test_support.rs` :: `ControlFixture` (L315-L444) — `ControlFixture::new().await` 自动读取 `AIAXIOM_DATABASE_URL`、建池、执行 `ensure_control_migrated`、创建临时 `storage`/`workspace` 与 `[7; 32]` 主密钥的 `ControlPlane`。
  - `apps/aiaxiom-server/src/server/ops.rs` :: `mod infra_tests` (L1004-L1161) — 标注 `#[ignore = "needs PostgreSQL: cargo xtask infra-test -p aiaxiom-server <过滤名>"]`，可直接读 `AIAXIOM_DATABASE_URL`、`ensure_control_migrated(&pool).await`、构造 `Args::try_parse_from(...)` 配合临时 `master_key_file`（base64 编码 32 字节）跑完整 `ops::run` 并查 `aiaxiom_control.audit_events` 验证审计行（测试结束注意清理自建的 `auth_providers` / `encrypted_secrets` / `audit_events` 行，避免污染同库其他用例）。
- **`control-plane` PostgreSQL 测试**：`crates/control-plane/src/oauth/tests.rs` (L13-L23, L261-L441) — 已有 `with_database` + `control_fixture` 与 `sample_github_upsert` 夹具。

---

### 四、看过但无关的文件
- `apps/aiaxiom-server/src/lib.rs` (L1-L5) — 仅导出 `server::{log_startup_failure, run}`，无需改动。
- `crates/control-plane/src/agent_move/schema.rs` (L261) — 已将 `auth_providers` 登记为 `Skip`，本次不改表结构。
- `crates/control-plane/src/format_check.rs` (L82, L417-L419) — 校验 `auth_providers.config`，本次不改 `AuthProviderConfig` 格式。
- `crates/control-plane/src/crypto.rs` (L126-L127) — 已有 `auth_provider_secret_aad`，由 `upsert_auth_provider_inner` 内部调用，无需改动。
- `docs/decision/0187-third-party-login.md` (L1-L25) — 已定稿 ADR，确认实例级通道与 host 运维边界，按规则不改 ADR。
