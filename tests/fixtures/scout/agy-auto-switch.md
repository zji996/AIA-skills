## L0

### 1. 落点
1. [crates/delegate/src/config.rs](crates/delegate/src/config.rs#L71-L84) :: `validate` (L71-L84) — 放行用户配置 `agy.autoSwitch`（5 字段），`repo == true` 报错；补缺省加载函数。
2. [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2195-L2444) :: `slot_model_pool_matches` / `check_pinned_slot_unavailability` / `AgyCredOverlay::prepare_active` / `prepare_for_session_in` (L2195-L2444) — 收口池匹配、移除 0% 静默回退、`prepare_active` 禁自动选号、在 `pinned == None` 接入池指针与三档选号（含在跑写手预扣）。
3. [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L1327-L1400) :: `refresh_all_quotas` / `cmd_list` / `cmd_use` / `command` (L1327-L1400, L2806-L2818, L3325-L3334, L4178-L4190) — 统一 15m `fetchedEpoch` + 跨进程单飞锁刷新入口、被动耗尽与 `authFailedAt` 写入、`cmd_use` 同步重置各池指针、新增只读 `pick` 子命令。
4. [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2161-L2193) :: `AgyAccountSwitch` / `format_account_switch_line` / `snapshot_json_opts` (L2161-L2193, L1787-L1845) — 扩展 `kind`（auto/handoff/fallback）一行格式化，快照附 `autoSwitch` 摘要。
5. [crates/delegate/src/agents.rs](crates/delegate/src/agents.rs#L1648-L1734) :: `run_agent_cancellable` (L1648-L1734, L2234-L2247, L2495-L2530) — 落 `agy_account_auto` 事件（run log + `events::emit`）与 `meta.json` 切换记录，退出时按严格额度耗尽错误写被动标记。
6. [crates/delegate/src/supervise.rs](crates/delegate/src/supervise.rs#L1021-L1217) :: `resume_transient` / `escalate` / `supervise` / `compute_tokens` (L1021-L1094, L1200-L1217, L1901-L1980, L2312-L2344) — 写手/只读中途耗尽同进程换号接力（至多 1 次，`agy_account_handoff`）、`escalate` 设 `freshAgySession` 并清旧切换记录、`compute_tokens` 按 session 分段求和、`AuthRefresh` 探本 run 钉号并写 `authFailedAt`。
7. [crates/delegate/src/review.rs](crates/delegate/src/review.rs#L772-L852) :: `check_extra_quota` / `run_accept_and_review` (L772-L852, L3506-L3590, L3642-L3645, L3891-L3984, L5071-L5090) — 非续接审查设 `freshAgySession` 并清旧切换记录、主审耗尽先完整模板换号重审 1 次再 fallback、`check_extra_quota` 改看本池当前号并复用统一池匹配、返工轮接力附 `verdict_file`。
8. [crates/delegate/src/usage.rs](crates/delegate/src/usage.rs#L1897-L1911) :: `model_matches_pool` (L1897-L1911) — 改调统一池匹配函数。
9. [crates/delegate-tui/src/main.rs](file:///home/zji/.cache/delegate-tui/src/main.rs#L274-L316) :: `QuotaCmd` / `main` / `data.rs` / `model.rs` / `ui.rs` / `ui_quota.rs` — 定时刷新改走 15m 节流入口（仅手动 `Q` 强制）、展示 `autoSwitch` 摘要与 `auto`/`handoff` 一行。
10. [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L4886-L4912) & [tests/test_delegate_agy_sticky.py](tests/test_delegate_agy_sticky.py#L222-L469) — 既有测试断言「缓存 0% 静默回退到 active」，移除 0% 回退后须同步更新。
11. [skills/delegate/references/agy-accounts.md](skills/delegate/references/agy-accounts.md#L1-L45) & `CHANGELOG.md` — 补自动选号一节（禁写主机/账号/邮箱）与 `## Unreleased` 一条。

### 2. 跨文件关系与测试命令
- **调用链**：所有会话入口（`launch` → `supervise`、`review`、`radar`）均收口到 `agents::run_agent_cancellable` → `AgyCredOverlay::prepare_for_session`；在跑写手计数可复用 `launch::active_machine(&state_dir())`；`meta.json` 的 `agyAccountSwitch` / `reviewAgyAccountSwitch` 自动透传至 `runs::status_json` → `output.rs`、`card.rs`、`index.rs` → `delegate-tui`。TUI 定时与手动刷新均经子进程调 `delegate agy account list --quota --json`。
- **测试命令**：
  - `cd crates/delegate && cargo test agy_accounts` / `cargo test compute_tokens` / `cargo test config`
  - `cd crates/delegate-tui && cargo test`
  - `python3 scripts/run-tests.py -k agy_sticky`
  - 完整门禁：`./scripts/verify.sh`

### 3. 与说明不符之处
1. **方案文件位置**：`.local/plan/agy-auto-switch-plan.md` 与 `agy-auto-switch-carbon-review.md` 被 git 忽略，不在当前 worktree 内，位于主仓库 `/home/zji/.local/share/aia-skills/.local/plan/`。
2. **TUI 刷新路径**：方案称 TUI 直接调 `refresh_all_quotas`，实际 `delegate-tui` 是独立 crate，通过子进程执行 `delegate agy account list --quota --json`（`crates/delegate-tui/src/data.rs:192-196`）；要区分定时节流与手动 `Q` 强制，需在 `agy account list` 区分（如非强制参数或定时走节流标志）。
3. **既有测试冲突**：`agy_accounts.rs:4886-4912` 与 `tests/test_delegate_agy_sticky.py:222,366` 均强断言「钉住槽位缓存 0% 时静默切回 active」，移除该分支后这 3 处测试会红，需同步改写。

### 4. 未核实项
- **官方 `agy` 额度耗尽精确报错串**：未实测真实 `agy` 耗尽输出；实现被动标记前可翻本机历史 run 的 `events.jsonl` / `stderr.log` 核对 `turn_error` 文案，并对照 `transient.rs:104-106` 与 `review.rs:660-673` 确保不含瞬时 `429`。
- **`probe::recovered` 传钉住账号的签名变动影响**：`supervise.rs:1045,1118` 与 `probe.rs:295` 均调 `probe::recovered`，未排查 `probe::recovered` 其余调用点（需 `rg "probe::recovered" crates/delegate/src/`）。

---

## L1

### 一、完整调用表与数据结构位置

#### 1. 配置与模型解析
- [crates/delegate/src/config.rs](crates/delegate/src/config.rs#L49-L84) :: `user_path` (L49-L55)、`read_file` (L57-L69)、`validate` (L71-L84)
  - 签名：`pub fn validate(value: &Value, path: &Path, repo: bool) -> Res<()>`
  - 现状：L75-L84 只允许 `agy.agent`；可参照 L73 `crate::remote::validate(value.get("remote"), repo)?` 与 L74 `crate::disk::validate_config(value.get("disk"), repo)?` 的模式，对 `agy.autoSwitch` 在 `repo == true` 时拒绝。
- [crates/delegate/src/models.rs](crates/delegate/src/models.rs#L108-L145) :: `resolve_model` (L108-L145)
  - 签名：`pub fn resolve_model(input: &str, config: &Value, listed: &[PiModel]) -> Res<String>`
  - 用途：`delegate agy account pick --model <别名>` 解析模型别名到完整模型 ID（只读场景注意不触发外部进程，可直接读配置别名映射或传入空 `listed` 时留意 `check_full` 行为）。

#### 2. 账号管理、额度缓存、锁与选号核心（`agy_accounts.rs`）
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L212-L227) :: `acquire_accounts_lock` (L212-L227) — 锁 `~/.config/delegate/agy-accounts/.lock`（`File::lock` 排他锁）。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L695-L720) :: `quota_cache_path` (L695)、`load_quota_cache` (L700) — 缓存路径 `<state>/agy-quota.json`。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L1327-L1477) :: `refresh_all_quotas` (L1327-L1477)
  - 签名：`pub fn refresh_all_quotas(agy_bin_opt: Option<&str>) -> Res<Value>`
  - 现状：并发 `thread::scope` 跑 `fetch_quota_for_slot`，仅最后写缓存持 `locks::acquire_state(&state_dir())`（L1371）。注意 `locks.rs:99-109` 严禁在持有 `LockKind::State` 时跑子进程或 sleep，因此跨进程单飞锁 `<state>/agy-quota-refresh.lock` 必须独立于 `State` 锁且在其外层。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L1787-L1845) :: `snapshot_json_opts` (L1787-L1845)、`snapshot_json` (L567)、`snapshot_json_fast` (L1531) — 供 `list`/`status`/`index.rs`/TUI 读取账号与额度快照。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2161-L2193) :: `AgyAccountSwitch` (L2161-L2165)、`PreparedAgyAccount` (L2168-L2172)、`format_account_switch_line` (L2176-L2193)
  - 现状：`AgyAccountSwitch { from, to, reason }`；`format_account_switch_line(sw: &Value, is_reviewer: bool) -> Option<String>` 被 `output.rs:1036,1041` 和 `card.rs:3862,3867` 共用。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2195-L2214) :: `slot_model_pool_matches` (L2195-L2214)
  - 三套池匹配分裂位置：① 此处（漏 `neon/carbon/hydrogen/flash/pro/codex/haiku` 且 `model=""` 返回 `true`）；② [crates/delegate/src/usage.rs](crates/delegate/src/usage.rs#L1897-L1911) :: `model_matches_pool`；③ [crates/delegate/src/review.rs](crates/delegate/src/review.rs#L820-L823) 内联 `pool_low.contains("claude") || pool_low.contains("gpt")`。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2221-L2304) :: `check_pinned_slot_unavailability` (L2221-L2304) — L2278-L2301 包含 0% 静默回退逻辑；L2290-L2294 空 `resetAt` 被视为未重置。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2319-L2550) :: `AgyCredOverlay` (L2319-L2327)、`prepare_active` (L2335-L2338)、`prepare_for_session` (L2345-L2372)、`prepare_for_session_in` (L2393-L2550)
  - 注意：`prepare_active()` 直接调 `Self::prepare_for_session(None, "")`（L2336），被 `probe.rs:295` 调用。
  - L2471-L2516 仅当选中槽位 == 全局 `active_id` 时才从实盘 `~/.gemini` 回同步凭据；非活跃槽位绝不碰实盘 `~/.gemini`。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2806-L2818) :: `cmd_list` (L2806-L2818) — `--quota` 调 `refresh_all_quotas`。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L3230-L3378) :: `cmd_use` (L3230-L3378) — L3328 写 `state["active"] = json!(target_id)` 并 `save_state`；需在此同步重置各池指针为 `target_id`。
- [crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L4148-L4191) :: `command` (L4148-L4191) — 子命令路由（`list`/`status`/`add`/`use`/`note`/`remove`/`import-agy-swap`/`restore-entry`），在此加 `"pick"`。

#### 3. 进程启动、错误捕获、在跑计数与追加消息
- [crates/delegate/src/agents.rs](crates/delegate/src/agents.rs#L1648-L1734) :: `run_agent_cancellable` (L1648-L1734) — 读 `isReviewer`/`isExtraReviewer`/`freshAgySession`/`ownerRunDir` 决定 `pin_key`/`switch_key`/`pinned_id`；调 `prepare_for_session`；回写 `meta_path` 并写 `agy_account_switch` 事件到 run log（L1717-L1730）。
- [crates/delegate/src/agents.rs](crates/delegate/src/agents.rs#L2234-L2247) :: 事件循环中 `"turn"` 会 `answer.clear()`，`"turn_error"` 置 `turn = json!({"stopReason":"error"})`；L2495-L2498 调 `overlay.finish()` 并释放 overlay。
- [crates/delegate/src/agy.rs](crates/delegate/src/agy.rs#L255-L295) :: `Filter::events` (L255-L295) — `status != "SUCCESS"` 时不产出 `result`，只产出 `turn` + `turn_error`（取 `r["error"]`/`finished_reason`/`status`）+ `settled`。
- [crates/delegate/src/transient.rs](crates/delegate/src/transient.rs#L62-L113) :: `kind` (L62-L113) — L75-L79 识别 `Kind::AuthRefresh`；L104-L106 识别 `insufficient_quota`/`quota exceeded`/`insufficient credit` 为非瞬态（返回 `None`）。
- [crates/delegate/src/launch.rs](crates/delegate/src/launch.rs#L2624-L2653) :: `reply` (L2624-L2653) — 常规续接继承 `agyAccount`，`--fresh` 不继承。
- [crates/delegate/src/launch.rs](crates/delegate/src/launch.rs#L2744-L2776) :: `active_machine` (L2744-L2762) / `machine_runs` (L2766-L2776) — 读 `<state>/*.slot` 列出在跑 run 目录（`machine_runs` 严格只读不删文件，适合只读命令 `pick` 与选号计数）。
- [crates/delegate/src/pending_reply.rs](crates/delegate/src/pending_reply.rs#L565-L612) :: `take_or_close_in_run` (L565-L612) — 消费后的追加消息留在 `<run>/appends.json`（`deliveredAt != null`、`cancelledAt == null`），写手接力时可直接读取已消费项避免丢失。

#### 4. 监督循环、交接、审查与 token 统计
- [crates/delegate/src/supervise.rs](crates/delegate/src/supervise.rs#L735-L756) :: `continuation_with_file` (L735-L756) — 清除 `freshAgySession`（L751），保证同会话返工/续做严格粘滞。
- [crates/delegate/src/supervise.rs](crates/delegate/src/supervise.rs#L1021-L1094) :: `resume_transient` (L1021-L1094) — `Kind::AuthRefresh` 路径调 `probe::recovered`（L1045）并设 `auth_failed = true`。
- [crates/delegate/src/supervise.rs](crates/delegate/src/supervise.rs#L1168-L1235) :: `escalate` (L1168-L1217)、`sync_agy_account_fields` (L1219-L1235) — `escalate` 置 `meta["fork"] = Value::Null`（L1205），需补 `freshAgySession = true` 并清 `agyAccountSwitch`（注意 `sync_agy_account_fields` 在 L1214 会把磁盘上的旧字段拷回 `meta`，清除须同步写盘）。
- [crates/delegate/src/supervise.rs](crates/delegate/src/supervise.rs#L1894-L1980) :: 主尝试、transient/stalled/policy 恢复、`escalate` 流程（写手/只读中途耗尽接力落点）。
- [crates/delegate/src/supervise.rs](crates/delegate/src/supervise.rs#L2075-L2101) & [L2312-L2344](crates/delegate/src/supervise.rs#L2312-L2344) :: `compute_tokens` — 现状对 `agy` 直接取 `turns.last()`；`events.jsonl` 中每次会话切换均有 `{"e":"session","id":...,"attempt":...}`（`agents.rs:1644`），可按 `session` 分段取各段末 `turn` 求和。
- [crates/delegate/src/review.rs](crates/delegate/src/review.rs#L659-L676) :: `classify_unavailable_reason` (L659-L676) — 把 `429`/`rate limit`/`capacity` 归为 `"额度用尽"`，被动标记不可复用此表。
- [crates/delegate/src/review.rs](crates/delegate/src/review.rs#L772-L852) :: `check_extra_quota` (L772-L852) — 由 L3642-L3644 调用，目前传 `agy_accounts::active_account_id()` 且未判 `resetAt`。
- [crates/delegate/src/review.rs](crates/delegate/src/review.rs#L3500-L3591) :: `fork_target` 计算与 `build_reviewer_meta` (L1680) — `fork_target.is_none()` 时未设 `freshAgySession = true`；L3550-L3578 为完整模板（含 `rubric` 与 `task_brief`）。
- [crates/delegate/src/review.rs](crates/delegate/src/review.rs#L3891-L3984) :: 审查员 `fallback` 逻辑 — 换号重审应插在 L3891 之前，参照 L3937-L3977 构造 `fork_target = None` 完整模板 + `freshAgySession = true`。
- [crates/delegate/src/review.rs](crates/delegate/src/review.rs#L4994-L5090) :: 返工轮 `author_next` 执行入口（返工耗尽接力需附本轮 `verdict_file`）。

#### 5. 状态透传、卡片与 TUI
- [crates/delegate/src/runs.rs](crates/delegate/src/runs.rs#L976-L986) & [L1055-L1057](crates/delegate/src/runs.rs#L1055-L1057) — 自动把 `meta`/`summary` 中的 `agyAccountSwitch`/`reviewAgyAccountSwitch` 透传到 `status` JSON。
- [crates/delegate/src/output.rs](crates/delegate/src/output.rs#L1035-L1044) & [crates/delegate/src/card.rs](crates/delegate/src/card.rs#L3667-L3673) / [L3861-L3870](crates/delegate/src/card.rs#L3861-L3870) — 直接调 `agy_accounts::format_account_switch_line`。
- [crates/delegate-tui/src/model.rs](file:///home/zji/.cache/delegate-tui/src/model.rs#L703-L719) :: `AgyAccountSwitchInfo` (L703-L719)、`AgyAccountsState` (L1634-L1644)。
- [crates/delegate-tui/src/data.rs](file:///home/zji/.cache/delegate-tui/src/data.rs#L178-L208) :: `fetch_agy_accounts`、`refresh_agy_quota`、`parse_agy_account_switch` (L1433-L1451)、`parse_agy_accounts_value` (L3888)。
- [crates/delegate-tui/src/main.rs](file:///home/zji/.cache/delegate-tui/src/main.rs#L260-L318) & [L642-L692](file:///home/zji/.cache/delegate-tui/src/main.rs#L642-L692) & [L913](file:///home/zji/.cache/delegate-tui/src/main.rs#L913) — `quota_refresh_every` 当前硬编码 600s；定时（L690）与手动 `Q`（L913）目前共用 `QuotaCmd::RefreshAll`。
- [crates/delegate-tui/src/ui_quota.rs](file:///home/zji/.cache/delegate-tui/src/ui_quota.rs#L549-L559) & [crates/delegate-tui/src/ui.rs](file:///home/zji/.cache/delegate-tui/src/ui.rs#L1803-L1818) — 账号页顶部 meta 行与 run 详情切换行渲染。

---

### 二、可参照的同类实现
- **仅用户配置校验**：[crates/delegate/src/config.rs](crates/delegate/src/config.rs#L73-L74) 对 `remote`/`disk` 的 `repo: bool` 校验模式。
- **审查员完整模板重建与 `freshAgySession`**：[crates/delegate/src/review.rs](crates/delegate/src/review.rs#L3937-L3981)（`review.fallback` 在 `fork_target.is_some()` 时重生成完整增量 prompt、设 `fb_meta["freshAgySession"] = json!(true)` 并回拷 `usage.json`）。
- **只读快照与人类/JSON 双输出**：[crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L2806-L2860) (`cmd_list`)，可作为 `cmd_pick` 的参数解析（`parse_simple`）与输出格式范本。
- **锁顺序规范**：[crates/delegate/src/locks.rs](crates/delegate/src/locks.rs#L34-L65) `LockKind` 枚举与 `assert_not_holding_state_lock`。

---

### 三、现成测试夹具写法
1. **Rust `agy_accounts` 注入测试**：[crates/delegate/src/agy_accounts.rs](crates/delegate/src/agy_accounts.rs#L4323-L4336) `make_test_token_json` 与 [L4800-L4920](crates/delegate/src/agy_accounts.rs#L4800-L4920) `sticky_session_keeps_pinned_slot_and_only_falls_back_to_active_when_unavailable` — 用 `TempHome::create()`、`save_state`、直接构造 `quota_cache` JSON 与 `now_ep` 调 `prepare_for_session_in`，全程无真实 `agy`。
2. **Rust `compute_tokens` 单测**：[crates/delegate/src/supervise.rs](crates/delegate/src/supervise.rs#L2347-L2375) `token_tests` 与 [L2421-L2446](crates/delegate/src/supervise.rs#L2421-L2446) `continuation_with_file_clears_fresh_agy_session`。
3. **Python 黑盒粘滞与脚本化 fake `agy`**：[tests/test_delegate_agy_sticky.py](tests/test_delegate_agy_sticky.py#L20-L169) — 提供 `write_user_config`、`_seed_three_accounts`（经 `import-agy-swap` 建 3 槽位）与 `_install_scripted_agy`（可按调用次数模拟耗尽报错、检查 overlay 凭据邮箱、记录 `agy-calls.jsonl`）。
4. **TUI 解析与快照测试**：`crates/delegate-tui/tests/data_parsing.rs` 与 `crates/delegate-tui/tests/ui_snapshots.rs`（以及 `data.rs:4261` 单测）。

---

### 四、看过但无关的文件
- [crates/delegate/src/hook_check.rs](crates/delegate/src/hook_check.rs) 与 [skills/delegate/hooks/claude-code-stop.py](skills/delegate/hooks/claude-code-stop.py) — `rg` 确认均不消费 `agy_account_*` 或额度事件，无需改动。
- [crates/delegate/src/index.rs](crates/delegate/src/index.rs#L708-L748) — 直接透传 `agy_accounts::snapshot_json_fast()`，无需额外字段 plumbing。
- [crates/delegate/src/probe.rs](crates/delegate/src/probe.rs#L294-L301) — 只调 `AgyCredOverlay::prepare_active()`；只要 `prepare_active()` 禁自动选号即可（若 `AuthRefresh` 改为探钉住槽位，则才需给 `probe::recovered` 传可选槽位）。
